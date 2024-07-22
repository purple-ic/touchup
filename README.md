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

TODO(launcher) or [install through cargo](#cargo-install)

## cargo install

TouchUp can be installed through cargo using `cargo install touchup`. Note that
this requires you have a [custom FFmpeg installation](https://github.com/zmwangx/rust-ffmpeg/wiki/Notes-on-building).

This method will also allow you to [specify custom feature flags](#feature-flags).

Alternatively, on Linux, you can build FFmpeg together with TouchUp using
`cargo install touchup -F ffmpeg_next/build`. Note that you'll have to manually
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