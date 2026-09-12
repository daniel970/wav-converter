pub fn completion_message(
    ok: usize,
    failed: usize,
    order_failed: bool,
    name_warning: bool,
) -> String {
    let mut message = format!("변환 성공 {ok}개 · 실패 {failed}개");
    if order_failed {
        message.push_str("\n순서 정리에 실패했습니다. 프로그램 로그를 확인해 주세요.");
    }
    if name_warning {
        message.push_str("\n비영문 파일명 경고가 있습니다. 프로그램을 확인해 주세요.");
    }
    message
}

/// Register this portable app for notifications for the current Windows user.
#[cfg(windows)]
pub fn show_completion(message: &str) -> anyhow::Result<()> {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};
    use winrt_notification::{Duration, Sound, Toast};
    const APP_ID: &str = "WavConverter.Desktop";
    let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
        .create_subkey(format!(r"Software\Classes\AppUserModelId\{APP_ID}"))?;
    key.set_value("DisplayName", &"WAV 일괄 변환기")?;
    Toast::new(APP_ID)
        .title("WAV 변환 작업 완료")
        .text1(message)
        .sound(Some(Sound::Default))
        .duration(Duration::Short)
        .show()
        .map_err(|error| anyhow::anyhow!("Windows 알림 전송 실패: {error}"))
}

#[cfg(not(windows))]
pub fn show_completion(_message: &str) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn completion_includes_counts_and_warnings() {
        assert_eq!(
            completion_message(26, 0, false, false),
            "변환 성공 26개 · 실패 0개"
        );
        let warning = completion_message(24, 2, true, true);
        assert!(warning.contains("실패 2개"));
        assert!(warning.contains("순서 정리에 실패"));
        assert!(warning.contains("비영문"));
    }
}
