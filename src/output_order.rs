//! Reinsert directory entries in playback order without rewriting audio data.
use crate::naming::natural_cmp;
use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub fn arrange_output(root: &Path, outputs: &[PathBuf]) -> Result<()> {
    let mut directories = BTreeSet::new();
    for output in outputs {
        if !output.starts_with(root) {
            bail!("출력 경로가 선택한 폴더 밖에 있습니다");
        }
        let mut parent = output.parent();
        while let Some(dir) = parent {
            if dir.is_dir() {
                directories.insert(dir.to_path_buf());
            }
            if dir == root {
                break;
            }
            parent = dir.parent();
        }
    }
    let mut directories: Vec<_> = directories.into_iter().collect();
    // Finish children before their parent directory entries move.
    directories.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for dir in directories {
        arrange_directory(&dir)?;
    }
    Ok(())
}

fn arrange_directory(dir: &Path) -> Result<()> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(".wav-converter-order-")
        {
            bail!(
                "이전 순서 정리의 복구 폴더가 있습니다: {}",
                entry.path().display()
            );
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            // Leave hidden/system entries, including System Volume Information, alone.
            if entry.metadata()?.file_attributes() & 6 != 0 {
                continue;
            }
        }
        entries.push(entry.file_name());
    }
    if entries.len() < 2 {
        return Ok(());
    }
    entries.sort_by(|a, b| {
        natural_cmp(&a.to_string_lossy(), &b.to_string_lossy()).then_with(|| a.cmp(b))
    });
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let stage = dir.join(format!(
        ".wav-converter-order-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir(&stage)?;
    let result = (|| -> Result<()> {
        for name in &entries {
            std::fs::rename(dir.join(name), stage.join(name))?;
        }
        for name in &entries {
            let destination = dir.join(name);
            if destination.exists() {
                bail!("정리 중 새 파일이 생겼습니다: {}", destination.display());
            }
            std::fs::rename(stage.join(name), destination)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        // Best-effort rollback. Never overwrite a concurrently created destination.
        for name in &entries {
            let source = stage.join(name);
            let destination = dir.join(name);
            if source.exists() && !destination.exists() {
                let _ = std::fs::rename(source, destination);
            }
        }
        let _ = std::fs::remove_dir(&stage); // only succeeds when empty
        return Err(error).with_context(|| {
            format!(
                "순서 정리 실패: {}. 남은 파일은 복구 폴더 {}에서 확인하세요",
                dir.display(),
                stage.display()
            )
        });
    }
    std::fs::remove_dir(&stage)?;
    // Check the actual enumeration order, not a separately sorted listing.
    let actual: Vec<_> = std::fs::read_dir(dir)?
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .map(|e| e.file_name())
        .filter(|name| entries.contains(name))
        .collect();
    if actual != entries {
        bail!(
            "파일은 보존했지만 파일시스템에서 요청한 기록 순서를 확인할 수 없습니다: {}",
            dir.display()
        );
    }
    Ok(())
}
