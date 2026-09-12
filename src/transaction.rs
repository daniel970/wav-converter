//! Keep each job reversible until the user can no longer cancel it.
//!
//! Backups live outside the output tree so directory-order rebuilding cannot
//! move them. Cleanup only removes individually tracked files and empty folders.
use crate::convert::{convert_file_cancellable, ConversionCancelled, OutputFormat};
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

struct Output {
    destination: PathBuf,
    original: Option<PathBuf>,
    published: bool,
}

pub struct OutputTransaction {
    root: PathBuf,
    workspace: PathBuf,
    outputs: Vec<Output>,
    temporary_files: Vec<PathBuf>,
    created_directories: Vec<PathBuf>,
    deferred_sources: Vec<PathBuf>,
    removed_sources: Vec<(PathBuf, PathBuf)>,
    sequence: usize,
    finished: bool,
    committed: bool,
}

impl OutputTransaction {
    pub fn new(root: &Path) -> Result<Self> {
        let root = absolute(root)?;
        // A volume root has no sibling. Ordering is disabled for that case and
        // the private workspace may safely be a child of the volume root.
        let mut ancestor = root.parent().unwrap_or(&root);
        while !ancestor.exists() {
            ancestor = ancestor
                .parent()
                .context("출력 폴더의 상위 경로가 없습니다")?;
        }
        validate_directory_chain(ancestor)?;
        let workspace = tempfile::Builder::new()
            .prefix(".wav-converter-job-")
            .tempdir_in(ancestor)
            .context("취소 복구용 임시 폴더 생성 실패")?
            .keep();
        Ok(Self {
            root,
            workspace,
            outputs: Vec::new(),
            temporary_files: Vec::new(),
            created_directories: Vec::new(),
            deferred_sources: Vec::new(),
            removed_sources: Vec::new(),
            sequence: 0,
            finished: false,
            committed: false,
        })
    }

    pub fn convert(
        &mut self,
        input: &Path,
        destination: &Path,
        format: OutputFormat,
        cancel: &AtomicBool,
    ) -> Result<()> {
        if self.finished {
            bail!("이미 종료된 변환 작업입니다");
        }
        let destination = absolute(destination)?;
        if !destination.starts_with(&self.root) || destination == self.root {
            bail!("출력 파일이 선택한 출력 폴더 밖에 있습니다");
        }
        let input = absolute(input)?;
        // Another planned output may have overwritten a later input's name.
        // Decode the original bytes retained in our journal in that case.
        let source = self
            .outputs
            .iter()
            .find_map(|entry| {
                if same_path(&entry.destination, &input) {
                    entry.original.clone()
                } else {
                    None
                }
            })
            .unwrap_or(input);
        let staged = self.next_path("converted", Some(Path::new("output.wav")));
        self.temporary_files.push(staged.clone());
        convert_file_cancellable(&source, &staged, format, cancel)?;
        if cancel.load(Ordering::Acquire) {
            return Err(ConversionCancelled.into());
        }
        self.ensure_parent(&destination)?;
        if let Ok(metadata) = fs::symlink_metadata(&destination) {
            if !metadata.file_type().is_file() {
                bail!(
                    "기존 출력이 일반 파일이 아닙니다: {}",
                    destination.display()
                );
            }
        }

        let existing = self
            .outputs
            .iter()
            .position(|entry| same_path(&entry.destination, &destination));
        let mut previous_generated = None;
        let index = if let Some(index) = existing {
            if !self.outputs[index].published {
                bail!(
                    "이 출력의 이전 저장 실패를 먼저 복구해야 합니다. 복구 폴더: {}",
                    self.workspace.display()
                );
            }
            // Two inputs can intentionally collapse to the same output name.
            // Preserve the previous result if publishing this conversion fails.
            if self.outputs[index].published {
                let previous = self.next_path("previous", Some(&destination));
                self.temporary_files.push(previous.clone());
                fs::rename(&destination, &previous).context("이전 변환 결과 임시 보관 실패")?;
                self.outputs[index].published = false;
                previous_generated = Some(previous);
            }
            index
        } else {
            let original = if destination.exists() {
                let backup = self.next_path("original", Some(&destination));
                self.write_recovery_mapping(&backup, &destination)?;
                fs::rename(&destination, &backup)
                    .with_context(|| format!("기존 출력 보관 실패: {}", destination.display()))?;
                Some(backup)
            } else {
                None
            };
            self.outputs.push(Output {
                destination: destination.clone(),
                original,
                published: false,
            });
            self.outputs.len() - 1
        };

        if let Err(error) = fs::rename(&staged, &destination) {
            let restore = previous_generated
                .as_ref()
                .or(self.outputs[index].original.as_ref());
            if let Some(restore) = restore {
                if let Err(restore_error) = fs::rename(restore, &destination) {
                    return Err(error).with_context(|| {
                        format!(
                            "출력 저장 및 기존 파일 복원 실패 ({restore_error}). 복구 파일: {}",
                            self.workspace.display()
                        )
                    });
                }
                if previous_generated.is_some() {
                    self.outputs[index].published = true;
                } else {
                    self.outputs[index].original = None;
                }
            }
            if existing.is_none() {
                // The original was put back; a later retry must journal it
                // again instead of mistaking it for one of our own outputs.
                self.outputs.remove(index);
            }
            return Err(error).context("변환 결과 저장 실패");
        }
        self.outputs[index].published = true;
        if let Some(previous) = previous_generated {
            let _ = fs::remove_file(previous);
        }
        Ok(())
    }

    /// Called only after a successful conversion in the original-replacement mode.
    pub fn defer_source_removal(&mut self, input: &Path) {
        // Actual deletion is postponed until commit; cancellation retains every
        // source, including sources for tracks already converted successfully.
        if let Ok(input) = absolute(input) {
            if !self.deferred_sources.iter().any(|p| same_path(p, &input)) {
                self.deferred_sources.push(input);
            }
        }
    }

    pub fn commit(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        // Never discard a journal that still contains the only original copy
        // after a publish/restore failure. Likewise, ordering or another process
        // may have moved a published output away since conversion succeeded.
        // Validate every result before moving sources or deleting any backup.
        for output in &self.outputs {
            if !output.published {
                bail!(
                    "저장에 실패한 출력을 먼저 복구해야 합니다: {}. 복구 폴더: {}",
                    output.destination.display(),
                    self.workspace.display()
                );
            }
            let metadata = fs::symlink_metadata(&output.destination).with_context(|| {
                format!(
                    "완료할 출력 파일 확인 실패: {}. 복구 폴더: {}",
                    output.destination.display(),
                    self.workspace.display()
                )
            })?;
            if !metadata.file_type().is_file() {
                bail!(
                    "완료할 출력이 일반 파일이 아닙니다: {}. 복구 폴더: {}",
                    output.destination.display(),
                    self.workspace.display()
                );
            }
        }
        // First move removable sources into the journal. If any move fails,
        // rollback can still restore all sources and replaced outputs.
        for source in self.deferred_sources.clone() {
            if self
                .outputs
                .iter()
                .any(|entry| entry.published && same_path(&entry.destination, &source))
            {
                continue;
            }
            if self
                .removed_sources
                .iter()
                .any(|(p, _)| same_path(p, &source))
            {
                continue;
            }
            let backup = self.next_path("source", Some(&source));
            self.write_recovery_mapping(&backup, &source)?;
            fs::rename(&source, &backup).with_context(|| {
                format!(
                    "변환 원본 정리 실패: {}. 복구 폴더: {}",
                    source.display(),
                    self.workspace.display()
                )
            })?;
            self.removed_sources.push((source, backup));
        }
        // Cleanup from here is irreversible. A cleanup failure must retain the
        // final outputs rather than letting Drop attempt a partial rollback.
        self.finished = true;
        self.committed = true;
        let mut errors = Vec::new();
        for output in &mut self.outputs {
            if let Some(backup) = &output.original {
                if let Err(error) = remove_file_if_present(backup) {
                    errors.push(format!("{}: {error}", backup.display()));
                } else {
                    output.original = None;
                }
            }
        }
        for (_, backup) in &self.removed_sources {
            if let Err(error) = remove_file_if_present(backup) {
                errors.push(format!("{}: {error}", backup.display()));
            }
        }
        self.cleanup_workspace(&mut errors);
        self.finish_errors(
            errors,
            "변환은 완료됐지만 복구용 임시 파일 정리에 실패했습니다",
        )
    }

    /// True after outputs become final, even if backup cleanup reports an error.
    pub fn is_committed(&self) -> bool {
        self.committed
    }

    pub fn rollback(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        let mut errors = Vec::new();
        for output in self.outputs.iter_mut().rev() {
            if output.published {
                if let Err(error) = remove_file_if_present(&output.destination) {
                    errors.push(format!("{}: {error}", output.destination.display()));
                    continue;
                }
                output.published = false;
            }
            if let Some(backup) = &output.original {
                if output.destination.exists() {
                    errors.push(format!(
                        "복원 경로에 파일이 있습니다: {}",
                        output.destination.display()
                    ));
                    continue;
                }
                if let Err(error) = fs::rename(backup, &output.destination) {
                    errors.push(format!("{}: {error}", output.destination.display()));
                } else {
                    output.original = None;
                }
            }
        }
        self.removed_sources.retain(|(source, backup)| {
            if source.exists() {
                errors.push(format!(
                    "원본 복원 경로에 파일이 있습니다: {}",
                    source.display()
                ));
                return true;
            }
            if let Err(error) = fs::rename(backup, source) {
                errors.push(format!("{}: {error}", source.display()));
                true
            } else {
                false
            }
        });
        for directory in self.created_directories.iter().rev() {
            if let Err(error) = fs::remove_dir(directory) {
                // An unrelated file may have been added while the job ran.
                // Never remove it, and never recursively remove a user folder.
                if error.kind() != std::io::ErrorKind::NotFound
                    && error.kind() != std::io::ErrorKind::DirectoryNotEmpty
                {
                    errors.push(format!("{}: {error}", directory.display()));
                }
            }
        }
        self.cleanup_workspace(&mut errors);
        if errors.is_empty() {
            self.finished = true;
        }
        self.finish_errors(errors, "취소 후 일부 파일 복원 또는 정리에 실패했습니다")
    }

    fn ensure_parent(&mut self, destination: &Path) -> Result<()> {
        let parent = destination.parent().context("출력 상위 폴더가 없습니다")?;
        let mut missing = Vec::new();
        let mut ancestor = parent;
        while !ancestor.exists() {
            missing.push(ancestor.to_path_buf());
            ancestor = ancestor.parent().context("출력 상위 폴더가 없습니다")?;
        }
        validate_directory_chain(ancestor)?;
        for directory in missing.into_iter().rev() {
            fs::create_dir(&directory)
                .with_context(|| format!("출력 폴더 생성 실패: {}", directory.display()))?;
            self.created_directories.push(directory);
        }
        Ok(())
    }

    fn next_path(&mut self, label: &str, original: Option<&Path>) -> PathBuf {
        self.sequence += 1;
        let mut name = format!("{label}-{}", self.sequence);
        if let Some(extension) = original.and_then(Path::extension) {
            name.push('.');
            name.push_str(&extension.to_string_lossy());
        }
        self.workspace.join(name)
    }

    fn write_recovery_mapping(&mut self, backup: &Path, original: &Path) -> Result<()> {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.workspace.join("recovery.txt"))?;
        writeln!(file, "{} -> {}", backup.display(), original.display())?;
        file.flush()?;
        Ok(())
    }

    fn cleanup_workspace(&self, errors: &mut Vec<String>) {
        for path in &self.temporary_files {
            if let Err(error) = remove_file_if_present(path) {
                errors.push(format!("{}: {error}", path.display()));
            }
        }
        // Keep the recovery mapping whenever any backup/cleanup is unresolved.
        if errors.is_empty() {
            if let Err(error) = remove_file_if_present(&self.workspace.join("recovery.txt")) {
                errors.push(error.to_string());
            }
        }
        if errors.is_empty() {
            if let Err(error) = fs::remove_dir(&self.workspace) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    errors.push(error.to_string());
                }
            }
        }
    }

    fn finish_errors(&self, errors: Vec<String>, message: &str) -> Result<()> {
        if errors.is_empty() {
            Ok(())
        } else {
            bail!(
                "{message}. 복구 폴더: {}\n{}",
                self.workspace.display(),
                errors.join("\n")
            )
        }
    }
}

impl Drop for OutputTransaction {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.rollback();
        }
    }
}

fn remove_file_if_present(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn absolute(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    bail!("잘못된 파일 경로입니다");
                }
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    Ok(normalized)
}

fn validate_directory_chain(path: &Path) -> Result<()> {
    for ancestor in path.ancestors() {
        if !fs::symlink_metadata(ancestor)?.file_type().is_dir() {
            bail!("출력 경로가 일반 폴더가 아닙니다: {}", ancestor.display());
        }
    }
    Ok(())
}

fn same_path(a: &Path, b: &Path) -> bool {
    #[cfg(windows)]
    {
        a.as_os_str().to_string_lossy().to_lowercase()
            == b.as_os_str().to_string_lossy().to_lowercase()
    }
    #[cfg(not(windows))]
    {
        a == b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_refuses_unresolved_original_backup_without_purging_or_removing_sources() {
        let fixture = tempfile::tempdir().unwrap();
        let source = fixture.path().join("source.flac");
        let destination = fixture.path().join("output.wav");
        fs::write(&source, b"source bytes").unwrap();
        let mut transaction = OutputTransaction::new(fixture.path()).unwrap();
        let backup = transaction.next_path("original", Some(&destination));
        fs::write(&backup, b"only remaining original copy").unwrap();
        // Deterministically reproduce both publish and immediate restore having
        // failed, without relying on a filesystem race or permission changes.
        transaction.outputs.push(Output {
            destination: destination.clone(),
            original: Some(backup.clone()),
            published: false,
        });
        transaction.defer_source_removal(&source);

        assert!(transaction.commit().is_err());
        assert!(!transaction.is_committed());
        assert_eq!(fs::read(&backup).unwrap(), b"only remaining original copy");
        assert_eq!(fs::read(&source).unwrap(), b"source bytes");
        transaction.rollback().unwrap();
        assert_eq!(
            fs::read(destination).unwrap(),
            b"only remaining original copy"
        );
    }

    #[test]
    fn commit_refuses_missing_or_non_file_output_before_removing_originals() {
        for replaced_by_directory in [false, true] {
            let fixture = tempfile::tempdir().unwrap();
            let source = fixture.path().join("source.flac");
            let destination = fixture.path().join("output.wav");
            fs::write(&source, b"source bytes").unwrap();
            let mut transaction = OutputTransaction::new(fixture.path()).unwrap();
            let backup = transaction.next_path("original", Some(&destination));
            fs::write(&backup, b"original destination").unwrap();
            transaction.outputs.push(Output {
                destination: destination.clone(),
                original: Some(backup.clone()),
                published: true,
            });
            transaction.defer_source_removal(&source);
            if replaced_by_directory {
                fs::create_dir(&destination).unwrap();
            }

            assert!(transaction.commit().is_err());
            assert!(!transaction.is_committed());
            assert_eq!(fs::read(&source).unwrap(), b"source bytes");
            assert_eq!(fs::read(&backup).unwrap(), b"original destination");
            if replaced_by_directory {
                fs::remove_dir(&destination).unwrap();
            }
            transaction.rollback().unwrap();
            assert_eq!(fs::read(destination).unwrap(), b"original destination");
        }
    }
}
