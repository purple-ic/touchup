# Data

This part of the repo is meant for serving data to TouchUp clients remotely.

Currently:

- [./youtube/](./youtube) is responsible for delivering YouTube API version information.

  This allows us to remotely update the upload endpoint used by TouchUp in case it ever changes,
  or just disable the YouTube features altogether.