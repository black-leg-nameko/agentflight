use anyhow::{Context, Result, bail};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Component, Path},
};
use walkdir::WalkDir;
use zip::{ZipArchive, ZipWriter, write::SimpleFileOptions};

const REQUIRED: [&str; 2] = ["manifest.json", "events.ndjson"];

pub fn export_run(run_dir: &Path, output: &Path) -> Result<()> {
    for name in REQUIRED {
        if !run_dir.join(name).is_file() {
            bail!("run is missing {name}");
        }
    }
    let file = File::create(output).with_context(|| format!("create {}", output.display()))?;
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);
    let mut checksums = String::new();
    for entry in WalkDir::new(run_dir)
        .min_depth(1)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
    {
        let name = entry
            .path()
            .strip_prefix(run_dir)?
            .to_string_lossy()
            .replace('\\', "/");
        if name == "checksums.txt" || name == "journal.log" || name == "mcp-capture.ndjson" {
            continue;
        }
        let data = fs::read(entry.path())?;
        checksums.push_str(&format!("{}  {}\n", blake3::hash(&data).to_hex(), name));
        zip.start_file(name, options)?;
        zip.write_all(&data)?;
    }
    zip.start_file("checksums.txt", options)?;
    zip.write_all(checksums.as_bytes())?;
    zip.finish()?;
    Ok(())
}

pub fn import_run(bundle: &Path, destination: &Path) -> Result<()> {
    let mut archive = ZipArchive::new(File::open(bundle)?)?;
    let checksums = {
        let mut file = archive
            .by_name("checksums.txt")
            .context("bundle has no checksums.txt")?;
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        text
    };
    let expected: std::collections::HashMap<_, _> = checksums
        .lines()
        .filter_map(|line| line.split_once("  "))
        .map(|(h, n)| (n.to_string(), h.to_string()))
        .collect();
    for required in REQUIRED {
        if !expected.contains_key(required) {
            bail!("bundle is missing {required}");
        }
    }
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let name = entry.name().to_string();
        if name == "checksums.txt" || entry.is_dir() {
            continue;
        }
        let path = Path::new(&name);
        if path.is_absolute()
            || path.components().any(|c| {
                matches!(
                    c,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            bail!("unsafe bundle path: {name}");
        }
        let mut data = Vec::new();
        entry.read_to_end(&mut data)?;
        let actual = blake3::hash(&data).to_hex().to_string();
        if expected.get(&name) != Some(&actual) {
            bail!("checksum mismatch: {name}");
        }
        let output = destination.join(path);
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(output, data)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bundle_round_trip() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let source = tmp.path().join("source");
        fs::create_dir(&source)?;
        fs::write(source.join("manifest.json"), "{}")?;
        fs::write(source.join("events.ndjson"), "")?;
        let archive = tmp.path().join("run.afrun");
        export_run(&source, &archive)?;
        let dest = tmp.path().join("dest");
        import_run(&archive, &dest)?;
        assert_eq!(fs::read(dest.join("manifest.json"))?, b"{}");
        Ok(())
    }
}
