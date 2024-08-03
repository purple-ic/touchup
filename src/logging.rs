use std::{env, panic, thread};
use std::any::Any;
use std::backtrace::{Backtrace, BacktraceStatus};
use std::cmp::min;
use std::ffi::OsStr;
use std::fmt::Display;
use std::fs::File;
use std::io::Write;
use std::sync::mpsc;
use std::sync::mpsc::Sender;

use backtrace::SymbolName;
use log::{info, LevelFilter, Log, Metadata, Record};
use simple_logger::SimpleLogger;

use crate::storage;
use crate::util::{CheapClone, FnDisplay};

struct Logger {
    inner: SimpleLogger,
    sender: Sender<String>,
}
// todo: properly handle IO errors when logging to file

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.inner.enabled(metadata)
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            let level = record.level();
            let target = if !record.target().is_empty() {
                record.target()
            } else {
                record.module_path().unwrap_or_default()
            };

            let thread = thread::current();
            let thread = thread.name();
            let thread = FnDisplay(|f| {
                if let Some(thread) = thread {
                    write!(f, "@{thread}")?;
                }
                Ok(())
            });
            let args = record.args();

            // todo: handle err?
            let _ = self.sender.send(
                format!(
                    "{level:<5} [{target}{thread}] {args}"
                )
            );
            self.inner.log(record)
        }
    }

    fn flush(&self) {
        self.inner.flush()
    }
}

fn test_backtraces() -> bool {
    let backtrace = Backtrace::capture();
    matches!(backtrace.status(), BacktraceStatus::Captured)
}

fn full_backtrace_disabled() -> bool {
    let var = env::var_os("TOUCHUP_SAVE_FULL_BACKTRACE");
    var.as_deref() == Some(OsStr::new("true"))
}

pub fn init_logging() {
    let enable_backtraces = test_backtraces() && !full_backtrace_disabled();

    let base_level = if cfg!(debug_assertions) {
        LevelFilter::Trace
    } else {
        LevelFilter::Warn
    };
    let cap_level = |level: LevelFilter| {
        min(base_level, level)
    };

    let inner_logger = SimpleLogger::new()
        .with_colors(true)
        .with_threads(true)
        .with_level(base_level)
        .with_module_level("yup_oauth2::authenticator", cap_level(LevelFilter::Info))
        .with_module_level("eframe::native", cap_level(LevelFilter::Debug));

    log::set_max_level(inner_logger.max_level());

    let mut path = storage();
    path.push("log.txt");
    let (send, recv) = mpsc::channel();
    thread::spawn(move || {
        let mut file = File::create(path).unwrap();
        while let Ok(value) = recv.recv() {
            let _ = writeln!(&mut file, "{value}");
        }
    });
    let logger = Logger {
        inner: inner_logger,
        sender: send.cheap_clone(),
    };

    #[cfg(windows)]
    simple_logger::set_up_windows_color_terminal();

    log::set_boxed_logger(Box::new(logger)).expect("could not initialize logger");

    // also capture panics into the file
    // todo: prefer panic::update_hook
    //      depends on https://github.com/rust-lang/rust/issues/92649

    let old_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        use std::fmt::Write;
        // println!("{}", Backtrace::capture());

        let thread = thread::current();
        let thread = thread.name().unwrap_or("<unnamed>");
        let location = info.location();
        let location = FnDisplay(|f| {
            if let Some(l) = location {
                Display::fmt(l, f)
            } else {
                write!(f, "<unknown>")
            }
        });
        let msg = payload_str(info.payload());

        let mut str = format!(
            "PANIC thread '{thread}' at {location}:\n\t\t\t\t{msg}"
        );

        if enable_backtraces {
            #[derive(Copy, Clone, Debug, PartialEq, Eq)]
            enum BacktraceStage {
                Prefix,
                Content,
                Suffix,
            }
            use BacktraceStage::*;

            let str = &mut str;
            str.push_str("\n\t\t\tstack backtrace:\n");
            let mut i = 0usize;
            let mut prefix_len = 0usize;
            let mut suffix_len = 0usize;
            let mut stage = Prefix;

            backtrace::trace(|frame| {
                backtrace::resolve_frame(frame, |f| {
                    let name = match f.name() {
                        None => return,
                        Some(v) => v,
                    };
                    if let Some(name) = name.as_str() {
                        if stage == Content && name.contains("__rust_begin_short_backtrace") {
                            stage = Suffix
                        }
                        if stage == Prefix && name.contains("__rust_end_short_backtrace") {
                            stage = Content
                        }
                    }
                    match stage {
                        Prefix => {
                            prefix_len += 1;
                        }
                        Content => {
                            // don't write the __rust_end_short_backtrace frame
                            if i != 0 {
                                let location = f.filename().zip(f.lineno());
                                let location = FnDisplay(|f| {
                                    if let Some((filename, lineno)) = location {
                                        write!(f, "\n\t\t\t\t\t\t\tat {f}:{lineno}", f = filename.display())
                                    } else {
                                        Ok(())
                                    }
                                });

                                let _ = writeln!(str, "\t\t\t\t{index}: {name}{location}", index = i - 1);
                            }

                            i += 1;
                        }
                        Suffix => {
                            suffix_len += 1;
                        }
                    }
                });
                stage != Suffix
            });
            let _ = writeln!(str, "\t\t\tomitted {prefix_len} prefix frames ::: omitted {suffix_len} suffix frames");
        } else {
            let _ = write!(&mut str, "\n{}", Backtrace::capture());
        }

        let _ = send.send(str);
        old_hook(info)
    }));

    info!("Logger setup complete\n\t\t\tsave short backtraces: {enable_backtraces}")
}

fn payload_str(payload: &dyn Any) -> &str {
    if let Some(&str) = payload.downcast_ref::<&'static str>() {
        str
    } else if let Some(string) = payload.downcast_ref::<String>() {
        &string
    } else {
        "<no message>"
    }
}