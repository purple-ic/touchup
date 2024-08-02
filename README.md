# TouchUp

TouchUp is a little program for giving your clips some editing and then quickly
shipping them off to YouTube or exporting them to disk.

This app does not by itself take clips. That part is left up to different programs:
[OBS](https://obsproject.com/)'s replay feature is the indented way but you can do
whatever you want.

## TODO: pics

## Features

- Trim the video's start and end point
- Change the video's audio track (only one can be exported), or mute audio entirely
    - This is useful if you have multiple audio tracks (e.g. one for game+mic, and one for game+mic+discord)
      for choosing just one. Most ways to share your clips (YouTube, Discord, etc...) won't let the user
      change the audio tracks so you most likely want to do that yourself.
    - TouchUp currently doesn't support mixing together multiple tracks at a time.
- Export the edited clips to a configured folder
- Upload the edited clip to YouTube quickly

## Installation

TouchUp doesn't come with an installer. We simply distribute the executable and necessary files
in a zip file. Those zip files can be found [here](https://github.com/purple-ic/touchup/releases/).

Releases come in two different flavors:

- Default: comes with the usual features (i.e. YouTube uploads)
- Mini: only includes the base functionality of TouchUp

If you'd like to further configure the included [features](#feature-flags) or
you'd like to statically link to FFmpeg instead of doing so dynamically, you
can [compile TouchUp yourself](#cargo-install)

### Windows

Windows releases come with the executable (`touchup.exe`) and all necessary FFmpeg libraries.
TouchUp should just work if you launch it through the executable.

If you'd like, you can manually create a shortcut. You can also have TouchUp launch
automatically on startup by adding the shortcut to the folder that opens when you
press `Win + R` and type `shell:startup`.

### Linux

The Linux release doesn't come with any FFmpeg libraries and links to them dynamically.
Your distribution likely comes with FFmpeg already, but if it doesn't, it shouldn't be
too difficult to get it installed.

Header files are not required unless you're compiling TouchUp from source

## cargo install

TouchUp can be built & installed through cargo
using `cargo install --git https://github.com/purple-ic/touchup --tag ${version}`
(replace `${version}` with the desired version of TouchUp. remove the `--tag ${version}` part to build from the latest
commit).
Note that this requires you have
a [custom FFmpeg installation](https://github.com/zmwangx/rust-ffmpeg/wiki/Notes-on-building).

This method will also allow you to [specify custom feature flags](#feature-flags).

Alternatively, on Linux, you can build FFmpeg together with TouchUp
by adding `-F ffmpeg_next/build`. Note that you'll have to manually
specify the built libraries; building without libraries will result in an FFmpeg
build that can't process any usual codec. The build configuration options can be found
[here](https://github.com/zmwangx/rust-ffmpeg/blob/1922ed055f96c368628e5b543ec4c59ddfa01ff4/Cargo.toml#L32-L88).
All of these can be added with `-F ffmpeg_next/{feature name here}`.

FFmpeg can't be automatically built on Windows.

## Feature flags

If you are building from source or [through cargo install](#cargo-install), you may
specify custom feature flags by appending `-F {feature name here}` to the install
command.

Default features (such as `youtube`) can be disabled by also passing `--no-default-features`.

- `youtube` (enabled by default): enables support for uploading clips to YouTube. You
  may want to disable this to speed up compilation time or to not depend on the `async` feature (see below)
- `async` (required for `youtube`): spins up an async runtime. This may be somewhat taxing
  on your device and might decrease compilation time, but it'll most likely be fine.

## License

TouchUp code is under the [MIT license](./LICENSE), however the crates used to create the application
use different licenses. Note that used crates vary depending on the operating system
and [enabled features](#feature-flags).

For any official TouchUp distribution where the FFmpeg libraries are present, they are licensed under the
LGPLv2.1, though TouchUp usually links to FFmpeg dynamically and the user may change the libraries, including
to a version which isn't licensed under the LGPLv2.1