#![cfg(feature = "update")]

use std::env::current_exe;
use std::fs;
use std::fs::File;
use std::path::PathBuf;
use std::str::FromStr;

use futures_util::{FutureExt, StreamExt};
use http_body_util::BodyExt;
use hyper::{StatusCode, Uri};
use serde::{Deserialize, Serialize};
use tempfile::tempdir;
use tokio::io::AsyncWriteExt;
use tokio::task::spawn_blocking;
use zip::ZipArchive;

use crate::https_client;

// todo: deal with issues with the fs running out of space
// todo: display update progress to user
// todo: different update processes for different OSs
//          e.g. for linux we probably only want to replace the executable since for linux we depend on global packages instead of local dlls like in windows

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct UpdateInfo<'a> {
    version: [u32; 3],
    #[cfg(target_os = "windows")]
    windows_artifact: &'a str,
    #[cfg(target_os = "linux")]
    linux_artifact: &'a str,
}

impl<'a> UpdateInfo<'a> {
    fn artifact_for_current_os(&self) -> &'a str {
        #[cfg(target_os = "windows")]
        {
            self.windows_artifact
        }
        #[cfg(target_os = "linux")]
        {
            self.linux_artifact
        }
        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        compile_error!("automatic updates are not supported on this platform")
    }
}

const UPDATE_CHECK_URL: &str =
    "https://raw.githubusercontent.com/purple-ic/touchup/main/data/update.json";

pub async fn check_updates() -> Option<Uri> {
    let mut p = crate::VERSION.split('.');
    let current_version: [u32; 3] =
        [p.next(), p.next(), p.next()].map(|x| x.unwrap().parse().unwrap());
    assert!(p.next().is_none());

    let client = https_client();
    let update = client
        .get(Uri::from_str(UPDATE_CHECK_URL).unwrap())
        .await
        .unwrap()
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let update: UpdateInfo = serde_json::from_slice(&update).unwrap();
    if update.version > current_version {
        Some(
            Uri::from_str(&format!(
                "https://api.github.com/repos/purple-ic/touchup/actions/artifacts/{}/zip",
                update.artifact_for_current_os()
            ))
                .unwrap(),
        )
    } else {
        None
    }
}

pub async fn update(artifact_getter_uri: Uri) {
    println!("[update] artifact: {artifact_getter_uri}");

    let client = https_client();
    let r = client.get(artifact_getter_uri).await.unwrap();
    assert_eq!(r.status(), StatusCode::FOUND);
    let artifact_uri = r.headers().get("Location").unwrap().to_str().unwrap();
    let artifact = client
        .get(Uri::from_str(artifact_uri).unwrap())
        .await
        .unwrap();
    assert!(artifact.status().is_success());
    let mut artifact = artifact.into_body().into_data_stream();

    let temp = tempdir().unwrap();
    println!("[update] downloading zip");
    let mut path = temp.path().join("download.zip");
    // path = $temp/download.zip
    let mut download_file = tokio::fs::File::create(&path).await.unwrap();
    while let Some(chunk) = artifact.next().await {
        let chunk = chunk.unwrap();
        download_file.write_all(&chunk).await.unwrap();
    }
    println!("[update] extracting zip");
    spawn_blocking(|| {
        // f = $temp/download.zip
        let zip = File::create(&path).unwrap();
        path.pop();
        path.push("extract");
        // path = $temp/extract/
        fs::create_dir(&path).unwrap();

        ZipArchive::new(zip).unwrap().extract(&path).unwrap();

        println!("[update] copying extracted files");
        // path = $temp/extract
        let read_dir = fs::read_dir(&path).unwrap();
        let out_exe = current_exe().unwrap();
        assert_eq!(out_exe.file_stem().unwrap(), "touchup", "current executable is not named `touchup`");
        let exe_name = out_exe.file_name().unwrap();

        path.push(exe_name); // path = $temp/extract/$exe
        assert!(path.exists(), "executable doesn't exist in extracted files");
        path.pop(); // path = $temp/extract/

        let mut out_dir = out_exe.clone();
        // out_dir = $install/$exe
        out_dir.pop();
        // out_dir = $install/
        copy_dir_all(read_dir, &out_exe, &mut path, &mut out_dir);
        println!("[update] replacing exe");
        path.push(exe_name); // path = $temp/extract/$exe
        self_replace::self_replace(path).unwrap();
    })
        .await
        .unwrap();
}

fn copy_dir_all<'a>(mut dir: fs::ReadDir, exe_path: &PathBuf, in_path: &'a mut PathBuf, out_path: &'a mut PathBuf) {
    while let Some(entry) = dir.next() {
        let entry = entry.unwrap();
        let ty = entry.file_type().unwrap();
        if ty.is_file() {
            let name = entry.file_name();
            out_path.push(&name);
            // don't copy the exe path. that'll be done by replace_self
            if out_path == exe_path {
                out_path.pop();
                continue;
            }
            in_path.push(name);
            fs::copy(&in_path, &out_path).unwrap();
            in_path.pop();
            out_path.pop();
        } else if ty.is_dir() {
            let name = entry.file_name();
            in_path.push(&name);
            out_path.push(name);
            fs::create_dir(&out_path).unwrap();
            let read_dir = fs::read_dir(&in_path).unwrap();
            copy_dir_all(read_dir, &exe_path, in_path, out_path);
            in_path.pop();
            out_path.pop();
        }
    }
}
