//! Rebuild directory tables in an empty sibling, without rewriting audio data.
use crate::naming::natural_cmp;
use anyhow::{bail, Context, Result};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

pub fn effective_output_dir(selected: &Path, arrange: bool) -> PathBuf {
    if arrange && selected.parent().is_none() {
        selected.join("WAV")
    } else {
        selected.to_path_buf()
    }
}
pub struct OrderReport {
    pub physical_order_verified: bool,
}

pub fn arrange_output(root: &Path, outputs: &[PathBuf]) -> Result<OrderReport> {
    if root.parent().is_none() {
        bail!("카드 최상위는 교체할 수 없습니다. 원본 대체를 끄고 출력 폴더를 선택하면 WAV 폴더에 저장합니다.");
    }
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
    let physical = if root.exists() {
        uses_physical_order(root)?
    } else {
        false
    };
    let mut directories: Vec<_> = directories.into_iter().collect();
    directories.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for dir in directories {
        rebuild_directory(&dir, physical)?;
    }
    Ok(OrderReport {
        physical_order_verified: physical,
    })
}

fn rebuild_directory(dir: &Path, verify: bool) -> Result<()> {
    let parent = dir.parent().context("최상위 폴더는 교체할 수 없습니다")?;
    if std::fs::symlink_metadata(dir)?.file_type().is_symlink() {
        bail!("링크 폴더는 정리할 수 없습니다");
    }
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(".wav-converter-order-")
        {
            bail!(
                "이전 정리의 복구 폴더를 확인해 주세요: {}",
                entry.path().display()
            );
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
    let stage = parent.join(format!(
        ".wav-converter-order-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir(&stage)?;
    let old_empty = stage.with_extension("old");
    let names = |path: &Path| -> std::io::Result<Vec<std::ffi::OsString>> {
        std::fs::read_dir(path)?
            .map(|e| e.map(|e| e.file_name()))
            .collect()
    };
    let result = (|| -> Result<()> {
        // Unlike reinsertion, this destination contains no deleted directory slots.
        for name in &entries {
            std::fs::rename(dir.join(name), stage.join(name))?;
        }
        if verify && names(&stage)? != entries {
            bail!("새 폴더의 번호순 기록 검증에 실패했습니다");
        }
        // Rename the empty original aside. On removable media a successful
        // RemoveDirectory can remain delete-pending while another handle closes.
        std::fs::rename(dir, &old_empty)?;
        if dir.exists() {
            bail!("정리 중 출력 폴더가 새로 생성됐습니다");
        }
        std::fs::rename(&stage, dir)?;
        Ok(())
    })();
    if let Err(error) = result {
        if old_empty.exists() && !dir.exists() {
            let _ = std::fs::rename(&old_empty, dir);
        }
        let _ = std::fs::create_dir_all(dir);
        for name in &entries {
            let source = stage.join(name);
            let destination = dir.join(name);
            if source.exists() && !destination.exists() {
                let _ = std::fs::rename(source, destination);
            }
        }
        let _ = std::fs::remove_dir(&stage);
        let _ = std::fs::remove_dir(&old_empty);
        return Err(error).with_context(|| {
            format!(
                "순서 정리 실패: {}. 복구 폴더가 남았다면 {}에서 파일을 확인하세요",
                dir.display(),
                stage.display()
            )
        });
    }
    std::fs::remove_dir(&old_empty)?; // empty directory only
    if verify && names(dir)? != entries {
        bail!(
            "폴더 교체 후 실제 순서 검증에 실패했습니다: {}",
            dir.display()
        );
    }
    Ok(())
}

#[cfg(windows)]
fn uses_physical_order(path: &Path) -> Result<bool> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{GetVolumeInformationW, GetVolumePathNameW};
    let path: Vec<u16> = std::fs::canonicalize(path)?
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut volume = vec![0u16; 32768];
    let mut filesystem = [0u16; 64];
    unsafe {
        if GetVolumePathNameW(path.as_ptr(), volume.as_mut_ptr(), volume.len() as u32) == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if GetVolumeInformationW(
            volume.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            filesystem.as_mut_ptr(),
            filesystem.len() as u32,
        ) == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    let end = filesystem
        .iter()
        .position(|c| *c == 0)
        .unwrap_or(filesystem.len());
    Ok(matches!(
        String::from_utf16_lossy(&filesystem[..end])
            .to_uppercase()
            .as_str(),
        "FAT" | "FAT32" | "EXFAT"
    ))
}
#[cfg(not(windows))]
fn uses_physical_order(_path: &Path) -> Result<bool> {
    Ok(false)
}
