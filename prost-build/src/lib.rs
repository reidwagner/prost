#![doc(html_root_url = "https://docs.rs/prost-build/0.1.1")]

extern crate bytes;
extern crate curl;
extern crate prost;
extern crate prost_codegen;
extern crate tempdir;
extern crate zip;

use std::env;
use std::fs;
use std::io::{
    self,
    Cursor,
    Error,
    ErrorKind,
    Read,
    Result,
    Write,
};
use std::path::{
    Path,
    PathBuf,
};
use std::process::Command;

use bytes::Buf;
use curl::easy::Easy;
use zip::ZipArchive;

use prost::Message;
use prost_codegen::google::protobuf::FileDescriptorSet;

pub fn compile_protos<P>(protos: &[P],
                         includes: &[P],
                         service_generator: Option<&prost_codegen::ServiceGenerator>)
                         -> Result<()> where P: AsRef<Path> {
    let target = match env::var("OUT_DIR") {
        Ok(val) => PathBuf::from(val),
        Err(env::VarError::NotPresent) => return Err(Error::new(ErrorKind::Other,
                                                                "OUT_DIR environment variable not set")),
        Err(env::VarError::NotUnicode(..)) => return Err(Error::new(ErrorKind::InvalidData,
                                                                    "OUT_DIR environment variable")),
    };

    let tmp = tempdir::TempDir::new("proto-build")?;

    // TODO: We should probably emit 'rerun-if-changed=PATH' directives for
    // cargo, however according to
    // http://doc.crates.io/build-script.html#outputs-of-the-build-script if we
    // output any, those paths will replace the default crate root, which we
    // don't want. Figure out how to do it in an additive way, perhaps gcc-rs
    // has this figured out.

    // If the protoc directory doesn't already exist from a previous build,
    // create it, and extract the protoc release into it.
    let protoc_dir = target.join("protoc");
    if !protoc_dir.exists() {
        fs::create_dir(&protoc_dir)?;
        download_protoc(&protoc_dir)?;
    }

    let mut protoc = protoc_dir.join("bin");
    protoc.push("protoc");
    protoc.set_extension(env::consts::EXE_EXTENSION);

    let descriptor_set = tmp.path().join("proto-descriptor-set");

    let mut cmd = Command::new(protoc);
    cmd.arg("-I").arg(protoc_dir.join("include"))
       .arg("--include_imports")
       .arg("--include_source_info")
       .arg("-o").arg(&descriptor_set);

    for include in includes {
        cmd.arg("-I").arg(include.as_ref());
    }

    for proto in protos {
        cmd.arg(proto.as_ref());
    }

    let output = cmd.output()?;
    if !output.status.success() {
        return Err(Error::new(ErrorKind::Other,
                              format!("protoc failed: {}",
                                      String::from_utf8_lossy(&output.stderr))));
    }

    let mut buf = Vec::new();
    fs::File::open(descriptor_set)?.read_to_end(&mut buf)?;
    let len = buf.len();
    let descriptor_set = FileDescriptorSet::decode(&mut <Cursor<Vec<u8>> as Buf>::take(Cursor::new(buf), len))?;

    let modules = prost_codegen::generate(descriptor_set.file, service_generator);
    for (module, content) in modules {
        let mut filename = match module.last() {
            Some(filename) => PathBuf::from(filename),
            None => return Err(Error::new(ErrorKind::InvalidInput, ".proto must have a package")),
        };
        filename.set_extension("rs");
        let mut file = fs::File::create(target.join(filename))?;
        file.write_all(content.as_bytes())?;
        file.flush()?;
    }

    Ok(())
}

/// Downloads and unpacks the protoc package for the current architecture to the target path.
fn download_protoc(target: &Path) -> Result<()> {
    let url = protoc_url()?;
    let mut data = Vec::new();
    let mut handle = Easy::new();

    handle.url(url)?;
    handle.follow_location(true)?;
    {
        let mut transfer = handle.transfer();
        transfer.write_function(|new_data| {
            data.extend_from_slice(new_data);
            Ok(new_data.len())
        })?;
        transfer.perform()?;
    }

    let mut archive = ZipArchive::new(Cursor::new(data))?;

    for i in 0..archive.len()
    {
        let mut src = archive.by_index(i)?;

        let mut path = target.to_owned();
        path.push(src.name());

        if src.name().ends_with('/') {
            fs::create_dir(&path)?;
        } else {
            let mut dest = &mut fs::File::create(&path)?;
            io::copy(&mut src, &mut dest)?;

            #[cfg(unix)]
            fn convert_permissions(mode: u32) -> Option<fs::Permissions> {
                use std::os::unix::fs::PermissionsExt;
                Some(fs::Permissions::from_mode(mode))
            }
            #[cfg(not(unix))]
            fn convert_permissions(_mode: u32) -> Option<fs::Permissions> {
                None
            }
            if let Some(permissions) = src.unix_mode().and_then(convert_permissions) {
                fs::set_permissions(&path, permissions)?;
            }
        }
    }

    Ok(())
}

fn protoc_url() -> Result<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86")    => Ok("https://github.com/google/protobuf/releases/download/v3.3.0/protoc-3.3.0-linux-x86_32.zip"),
        ("linux", "x86_64") => Ok("https://github.com/google/protobuf/releases/download/v3.3.0/protoc-3.3.0-linux-x86_64.zip"),
        ("macos", "x86")    => Ok("https://github.com/google/protobuf/releases/download/v3.3.0/protoc-3.3.0-osx-x86_32.zip"),
        ("macos", "x86_64") => Ok("https://github.com/google/protobuf/releases/download/v3.3.0/protoc-3.3.0-osx-x86_64.zip"),
        ("windows", _)      => Ok("https://github.com/google/protobuf/releases/download/v3.3.0/protoc-3.3.0-win32.zip"),
        _ => Err(Error::new(ErrorKind::NotFound,
                            format!("no precompiled protoc binary for current the platform: {}-{}",
                                    env::consts::OS, env::consts::ARCH))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_download_protoc() {
        let dir = tempdir::TempDir::new("protoc").unwrap();
        download_protoc(dir.path()).unwrap();
    }
}
