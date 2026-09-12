// 릴리스 빌드에서는 콘솔 창이 같이 뜨지 않도록 함 (Windows).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::VecDeque;
use std::io::Write;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, Receiver};
use std::sync::{
    atomic::{AtomicBool, AtomicU8, Ordering},
    Arc,
};
use std::thread;

use eframe::egui;
use walkdir::WalkDir;
use wav_converter::convert::{is_audio_file, is_cancelled, OutputFormat};
use wav_converter::naming::{
    filename_warning, has_track_prefix, natural_cmp, numbered_wav_path, read_track_number,
};
use wav_converter::output_order::{arrange_output, effective_output_dir};
use wav_converter::transaction::OutputTransaction;

const MAX_UI_LOGS: usize = 1000;

fn diagnostic_path() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("wav-converter")
        .join("diagnostics.log")
}

fn diagnostic(message: &str) {
    if cfg!(test) {
        return;
    }
    let path = diagnostic_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{:?} {message}", std::time::SystemTime::now());
    }
}

fn main() -> eframe::Result<()> {
    let path = diagnostic_path();
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 5 * 1024 * 1024) {
        let _ = std::fs::rename(&path, path.with_extension("previous.log"));
    }
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        diagnostic(&format!(
            "PANIC: {info}\n{}",
            std::backtrace::Backtrace::force_capture()
        ));
        previous_hook(info);
    }));
    diagnostic(&format!(
        "APP START version={} pid={} exe={:?}",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        std::env::current_exe()
    ));
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([760.0, 620.0])
            .with_min_inner_size([640.0, 520.0]),
        ..Default::default()
    };

    let result = eframe::run_native(
        concat!(
            "WAV 일괄 변환기  v",
            env!("CARGO_PKG_VERSION"),
            " (drag-hl)"
        ),
        native_options,
        Box::new(|cc| {
            install_korean_font(&cc.egui_ctx);
            Ok(Box::new(App::default()))
        }),
    );
    diagnostic(&format!("APP EXIT: {result:?}"));
    result
}

/// 백그라운드 변환 스레드 → UI로 보내는 메시지.
enum Msg {
    Total(usize),
    Progress { done: usize, file: String },
    Log(String),
    Finished { ok: usize, failed: usize },
    Cancelled { cleanup_error: Option<String> },
    OrderError(String),
    NameWarning(String),
    OutputWarning(String),
}

/// The atomic phase closes the race between a final Cancel click and commit.
#[derive(Default)]
struct JobControl {
    phase: AtomicU8, // 0: cancellable, 1: cancellation requested, 2: committing
    cancel: AtomicBool,
    #[cfg(test)]
    after_conversion: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl JobControl {
    fn request_cancel(&self) -> bool {
        if self
            .phase
            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            self.cancel.store(true, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    fn cancelled(&self) -> bool {
        self.phase.load(Ordering::SeqCst) == 1
    }
    fn can_cancel(&self) -> bool {
        self.phase.load(Ordering::SeqCst) == 0
    }
    fn begin_commit(&self) -> bool {
        self.phase
            .compare_exchange(0, 2, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }
}

/// 진행 중인 변환 작업 상태.
struct Job {
    control: Arc<JobControl>,
    rx: Receiver<Msg>,
    total: usize,
    done: usize,
    current: String,
    order_error: Option<String>,
}

#[derive(Default)]
struct App {
    input_dir: Option<PathBuf>,
    output_dir: Option<PathBuf>,
    format: Format,
    /// 출력을 입력과 동일하게 (원본을 WAV로 대체).
    same_as_input: bool,
    remove_artist: bool,
    disable_track_prefix: bool,
    disable_ordering: bool,
    filename_warning: Option<String>,
    output_warning: Option<String>,
    job: Option<Job>,
    log: VecDeque<String>,
    summary: Option<String>,
    close_when_finished: bool,
    // 끌어다 놓은 폴더가 들어갈 대상 (클릭으로 고정 / 위치 판정 실패 시 폴백).
    drop_target: Zone,
    // 직전 프레임의 박스 영역 (드롭 위치 판정용).
    input_rect: Option<egui::Rect>,
    output_rect: Option<egui::Rect>,
}

/// 드롭 영역 종류.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Zone {
    #[default]
    Input,
    Output,
}

/// OutputFormat 기본값 래퍼 (Default 구현용).
struct Format(OutputFormat);
impl Default for Format {
    fn default() -> Self {
        Format(OutputFormat::Preserve)
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if ctx.input(|i| i.viewport().close_requested()) {
            if let Some(job) = &self.job {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                job.control.request_cancel();
                self.close_when_finished = true;
            }
        }
        // 드롭된 파일이 있으면 OS에서 마우스 좌표를 직접 조회(Windows)해 위치 판정.
        let has_drop = ctx.input(|i| !i.raw.dropped_files.is_empty());
        let drop_pos = if has_drop {
            os_cursor_in_points(ctx, frame)
        } else {
            None
        };
        self.handle_dropped_files(ctx, drop_pos);
        self.pump_messages(ctx);
        if self.close_when_finished && self.job.is_none() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        let hovering_files = ctx.input(|i| !i.raw.hovered_files.is_empty());

        // 파일을 드래그하는 동안: OS에서 커서 위치를 매 프레임 조회해 박스 강조.
        let drag_cursor = if hovering_files {
            ctx.request_repaint(); // 드래그 중 계속 다시 그려 커서를 추적.
            os_cursor_in_points(ctx, frame)
        } else {
            None
        };
        let hot_input = drag_cursor.is_some_and(|p| self.input_rect.is_some_and(|r| r.contains(p)));
        let hot_output = !self.same_as_input
            && drag_cursor.is_some_and(|p| self.output_rect.is_some_and(|r| r.contains(p)));

        self.draw_ui(ctx, hovering_files, hot_input, hot_output);
    }
}

impl App {
    fn draw_ui(
        &mut self,
        ctx: &egui::Context,
        hovering_files: bool,
        hot_input: bool,
        hot_output: bool,
    ) {
        let running = self.job.is_some();
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.heading("🎵 WAV 일괄 변환기");
                ui.add_space(6.0);
                ui.colored_label(
                    egui::Color32::from_rgb(120, 200, 120),
                    concat!("v", env!("CARGO_PKG_VERSION"), " (drag-hl)"),
                );
            });
            ui.label("폴더 안의 모든 음원을 WAV로 변환합니다. (하위 폴더 구조 그대로 복제)");
            ui.add_space(10.0);

            // 원본 대체 모드에서는 드롭 대상을 입력으로 강제.
            if self.same_as_input {
                self.drop_target = Zone::Input;
            }

            // ===== 안내 =====
            ui.small(if hovering_files {
                "⬇ 원하는 박스 위에 놓으세요."
            } else {
                "폴더를 원하는 박스 위로 끌어다 놓으세요. (또는 박스 안 📂 버튼으로 선택)"
            });

            ui.add_space(6.0);

            // ===== 좌우 큰 영역 (박스 클릭 = 드롭 대상 선택) =====
            let zone_height = 150.0;
            ui.columns(2, |cols| {
                // --- 왼쪽: 입력 폴더 ---
                let (rect, clicked, pick) = drop_zone(
                    &mut cols[0],
                    zone_height,
                    "📥 입력 폴더",
                    &self.input_dir,
                    "(폴더를 끌어다 놓거나 버튼으로 선택)",
                    !running,
                    hot_input,
                );
                self.input_rect = Some(rect);
                if clicked {
                    self.drop_target = Zone::Input;
                }
                if pick {
                    self.drop_target = Zone::Input;
                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        self.input_dir = Some(dir);
                    }
                }

                // --- 오른쪽: 출력 폴더 ---
                let out_enabled = !running && !self.same_as_input;
                let out_hint = if self.same_as_input {
                    "원본 대체 모드 — 출력 폴더 사용 안 함"
                } else {
                    "(폴더를 끌어다 놓거나 버튼으로 선택)"
                };
                let (rect, clicked, pick) = drop_zone(
                    &mut cols[1],
                    zone_height,
                    "💾 출력 폴더",
                    &self.output_dir,
                    out_hint,
                    out_enabled,
                    hot_output,
                );
                self.output_rect = Some(rect);
                if clicked {
                    self.drop_target = Zone::Output;
                }
                if pick {
                    self.drop_target = Zone::Output;
                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        self.output_dir = Some(dir);
                    }
                }
            });

            ui.add_space(10.0);

            // ===== 원본 대체 체크박스 =====
            ui.add_enabled_ui(!running, |ui| {
                ui.checkbox(
                    &mut self.same_as_input,
                    "출력을 입력과 동일하게 (원본 음원을 변환된 WAV로 대체)",
                );
            });
            if self.same_as_input {
                ui.colored_label(
                    egui::Color32::from_rgb(220, 80, 80),
                    "⚠ 입력 폴더의 원본 음원 파일이 WAV로 대체되고, 원본 파일은 삭제됩니다.",
                );
            }

            ui.add_space(8.0);

            // ===== 출력 규격 =====
            ui.add_enabled_ui(!running, |ui| {
                ui.checkbox(
                    &mut self.remove_artist,
                    "파일명에서 아티스트 제거 (번호 아티스트 - 곡제목 → 번호 곡제목)",
                );
            });
            ui.add_enabled_ui(!running, |ui| {
                let mut enabled = !self.disable_ordering;
                ui.checkbox(
                    &mut enabled,
                    "DAP 재생 순서 맞추기 (변환 후 출력 폴더·곡을 번호순으로 정리)",
                );
                self.disable_ordering = !enabled;
            });
            ui.add_enabled_ui(!running, |ui| {
                let mut enabled = !self.disable_track_prefix;
                ui.checkbox(
                    &mut enabled,
                    "번호 없는 파일명 앞에 트랙 번호(#) 붙이기 (1. 곡제목)",
                );
                self.disable_track_prefix = !enabled;
            });
            ui.small(
                "순서 정리는 실제 저장 폴더에 적용됩니다. DAP에서 해당 폴더를 열어 재생하세요.",
            );
            if !self.same_as_input {
                if let Some(selected) = &self.output_dir {
                    ui.small(format!(
                        "실제 저장 위치: {}",
                        effective_output_dir(selected, !self.disable_ordering).display()
                    ));
                }
            }
            ui.horizontal(|ui| {
                ui.label("출력 규격:");
                ui.add_enabled_ui(!running, |ui| {
                    egui::ComboBox::from_id_salt("format")
                        .selected_text(self.format.0.label())
                        .show_ui(ui, |ui| {
                            for f in OutputFormat::ALL {
                                ui.selectable_value(&mut self.format.0, f, f.label());
                            }
                        });
                });
            });

            ui.add_space(12.0);

            // ===== 변환 시작 =====
            let can_start = !running
                && self.input_dir.is_some()
                && (self.same_as_input || self.output_dir.is_some());
            ui.add_enabled_ui(can_start, |ui| {
                if ui
                    .add(egui::Button::new("▶  변환 시작").min_size(egui::vec2(150.0, 34.0)))
                    .clicked()
                {
                    if self.same_as_input {
                        // 원본 삭제 경고 확인.
                        let res = rfd::MessageDialog::new()
                            .set_level(rfd::MessageLevel::Warning)
                            .set_title("원본 삭제 경고")
                            .set_description(
                                "입력 폴더의 원본 음원 파일들이 변환된 WAV로 대체되고,\n\
                                 원본 파일은 삭제됩니다.\n\n\
                                 정말 진행하시겠습니까? 이 작업은 되돌릴 수 없습니다.",
                            )
                            .set_buttons(rfd::MessageButtons::YesNo)
                            .show();
                        if res == rfd::MessageDialogResult::Yes {
                            self.start_job(ctx);
                        }
                    } else {
                        self.start_job(ctx);
                    }
                }
            });

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(6.0);

            // ===== 진행 상황 =====
            if let Some(job) = &self.job {
                let frac = if job.total == 0 {
                    0.0
                } else {
                    job.done as f32 / job.total as f32
                };
                ui.add(egui::ProgressBar::new(frac).text(format!("{} / {}", job.done, job.total)));
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(job.control.can_cancel(), egui::Button::new("■ 취소"))
                        .clicked()
                    {
                        job.control.request_cancel();
                    }
                    if job.control.cancelled() {
                        ui.label("취소 중… 이번 작업의 결과물을 정리하고 기존 파일을 복구합니다.");
                    } else if !job.control.can_cancel() {
                        ui.label("완료 처리 중…");
                    } else {
                        ui.label(format!("변환 중: {}", job.current));
                    }
                });
                ui.small(
                    "취소하면 이번 작업에서 만든 파일을 삭제하고, 덮어쓰기 전 파일을 복구합니다.",
                );
            } else if let Some(summary) = &self.summary {
                ui.label(summary);
            }

            ui.add_space(8.0);

            // ===== 로그 =====
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for line in &self.log {
                        ui.monospace(line);
                    }
                });
        });
        if let Some(message) = &self.filename_warning {
            let mut close = false;
            egui::Window::new("⚠ 비영문 파일명 경고")
                .id(egui::Id::new("filename_warning"))
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .order(egui::Order::Foreground)
                .collapsible(false)
                .default_width(500.0)
                .show(ctx, |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(320.0)
                        .show(ui, |ui| {
                            ui.label(message);
                        });
                    close = ui.button("확인").clicked();
                });
            if close {
                self.filename_warning = None;
            }
        }
        if let Some(message) = &self.output_warning {
            let mut close = false;
            egui::Window::new("⚠ 작업 결과 확인")
                .id(egui::Id::new("output_warning"))
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .order(egui::Order::Foreground)
                .collapsible(false)
                .default_width(500.0)
                .show(ctx, |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(320.0)
                        .show(ui, |ui| {
                            ui.label(message);
                        });
                    close = ui.button("확인").clicked();
                });
            if close {
                self.output_warning = None;
            }
        }
    }
}

impl App {
    /// 드래그&드롭으로 들어온 폴더를 배치한다.
    /// `drop_pos`(OS에서 조회한 마우스 좌표)가 어느 박스 위면 그 박스로,
    /// 못 얻었거나 박스 밖이면 클릭으로 고른 대상(`drop_target`)으로.
    fn handle_dropped_files(&mut self, ctx: &egui::Context, drop_pos: Option<egui::Pos2>) {
        if self.job.is_some() {
            return;
        }
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        if dropped.is_empty() {
            return;
        }

        // 폴더 경로 결정 (파일이면 그 부모 폴더).
        let folder = dropped.iter().find_map(|f| f.path.clone()).and_then(|p| {
            if p.is_dir() {
                Some(p)
            } else {
                p.parent().map(|x| x.to_path_buf())
            }
        });
        let Some(folder) = folder else { return };

        let on_output = drop_pos.is_some_and(|p| self.output_rect.is_some_and(|r| r.contains(p)));
        let on_input = drop_pos.is_some_and(|p| self.input_rect.is_some_and(|r| r.contains(p)));

        let target = if on_output {
            Zone::Output
        } else if on_input {
            Zone::Input
        } else {
            // 위치를 못 얻었거나 박스 밖 → 클릭으로 고른 대상.
            self.drop_target
        };

        match target {
            Zone::Output if !self.same_as_input => self.output_dir = Some(folder),
            _ => self.input_dir = Some(folder),
        }
    }

    /// 백그라운드 스레드에서 온 메시지 처리.
    fn pump_messages(&mut self, ctx: &egui::Context) {
        let mut finished = false;
        if let Some(job) = &mut self.job {
            loop {
                match job.rx.try_recv() {
                    Ok(Msg::Total(n)) => job.total = n,
                    Ok(Msg::Progress { done, file }) => {
                        job.done = done;
                        job.current = file;
                    }
                    Ok(Msg::Log(line)) => {
                        self.log.push_back(line);
                        while self.log.len() > MAX_UI_LOGS {
                            self.log.pop_front();
                        }
                    }
                    Ok(Msg::Finished { ok, failed }) => {
                        self.summary = Some(if ok == 0 && failed == 0 {
                            "변환할 음원이 없습니다.".to_owned()
                        } else if failed == 0 {
                            format!("✅ 완료 — 성공 {ok}개, 실패 {failed}개")
                        } else {
                            format!("⚠ 일부 변환 실패 — 성공 {ok}개, 실패 {failed}개")
                        });
                        if let Some(error) = &job.order_error {
                            self.summary = Some(format!(
                                "변환 성공 {ok}개, 실패 {failed}개 · 순서 정리 실패: {error}"
                            ));
                        }
                        if failed > 0 || job.order_error.is_some() {
                            self.close_when_finished = false;
                            let message = format!(
                                "{}\n전체 작업이 성공한 상태가 아닙니다. 아래 파일별 오류 기록을 확인하세요.",
                                self.summary.as_deref().unwrap_or_default()
                            );
                            self.output_warning = Some(match self.output_warning.take() {
                                Some(previous) => format!("{message}\n\n{previous}"),
                                None => message,
                            });
                        }
                        if !cfg!(test) {
                            let mut message = wav_converter::notification::completion_message(
                                ok,
                                failed,
                                job.order_error.is_some(),
                                self.filename_warning.is_some(),
                            );
                            if self.output_warning.is_some() {
                                message.push_str(
                                    "\n작업 결과 경고가 있습니다. 프로그램을 확인해 주세요.",
                                );
                            }
                            thread::spawn(move || {
                                if let Err(error) =
                                    wav_converter::notification::show_completion(&message)
                                {
                                    diagnostic(&format!("NOTIFICATION ERROR: {error:#}"));
                                }
                            });
                        }
                        self.log
                            .push_back(format!("── 작업 완료: 성공 {ok}, 실패 {failed} ──"));
                        finished = true;
                        break;
                    }
                    Ok(Msg::Cancelled { cleanup_error }) => {
                        self.filename_warning = None;
                        self.output_warning = cleanup_error.as_ref().map(|error| format!(
                            "취소 과정에서 오류가 발생했습니다.\n{error}\n복구 폴더가 남았다면 내부 파일을 확인하세요."
                        ));
                        self.summary = Some(if let Some(error) = cleanup_error {
                            // Keep the window open so a failed rollback is visible.
                            self.close_when_finished = false;
                            format!("⚠ 취소 정리 실패: {error}")
                        } else {
                            "취소 완료 — 이번 작업의 결과물을 정리하고 기존 파일을 복구했습니다."
                                .to_owned()
                        });
                        self.log.push_back(self.summary.clone().unwrap());
                        finished = true;
                        break;
                    }
                    Ok(Msg::OrderError(error)) => {
                        self.log.push_back(format!("⚠ 순서 정리 실패: {error}"));
                        job.order_error = Some(error);
                    }
                    Ok(Msg::NameWarning(message)) => self.filename_warning = Some(message),
                    Ok(Msg::OutputWarning(message)) => self.output_warning = Some(message),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        self.close_when_finished = false;
                        let message =
                            "변환 작업이 예기치 않게 중단되었습니다. 오류 기록을 확인해주세요.";
                        self.summary = Some(message.to_owned());
                        diagnostic(message);
                        finished = true;
                        break;
                    }
                }
            }
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        while self.log.len() > MAX_UI_LOGS {
            self.log.pop_front();
        }
        if finished {
            self.job = None;
        }
    }

    /// 변환 작업 시작 (파일 목록을 먼저 수집 후 스레드 실행).
    fn start_job(&mut self, ctx: &egui::Context) {
        self.start_job_with_control(ctx, Arc::new(JobControl::default()));
    }

    fn start_job_with_control(&mut self, ctx: &egui::Context, control: Arc<JobControl>) {
        let input = self.input_dir.clone().unwrap();
        let selected_output = self.output_dir.clone();
        let output = if self.same_as_input {
            Some(input.clone())
        } else {
            self.output_dir
                .as_ref()
                .map(|p| effective_output_dir(p, !self.disable_ordering))
        };
        let fmt = self.format.0;
        let in_place = self.same_as_input;
        let remove_artist = self.remove_artist;
        let use_track_number = !self.disable_track_prefix;
        let arrange = !self.disable_ordering;

        self.log.clear();
        self.summary = None;
        self.filename_warning = None;
        self.output_warning = None;
        self.log.push_back(format!("입력: {}", input.display()));
        if in_place {
            self.log.push_back("모드: 원본 대체 (in-place)".to_string());
        } else if let Some(o) = &output {
            self.log.push_back(format!("출력: {}", o.display()));
        }

        diagnostic(&format!(
            "JOB version={} pid={} input={} selected_output={selected_output:?} effective_output={output:?} format={fmt:?} in_place={in_place} remove_artist={remove_artist} arrange={arrange} track_prefix={use_track_number}",
            env!("CARGO_PKG_VERSION"),
            std::process::id(),
            input.display()
        ));
        self.log
            .push_back(format!("오류 기록: {}", diagnostic_path().display()));
        let (tx, rx) = sync_channel::<Msg>(256);
        let ctx2 = ctx.clone();
        let worker_control = Arc::clone(&control);

        thread::spawn(move || {
            let control = worker_control;
            let root = output.as_ref().unwrap();
            let mut transaction: Option<OutputTransaction> = None;
            if !in_place {
                if let (Some(selected), Some(effective)) = (&selected_output, &output) {
                    match outside_output_warning(selected, effective) {
                        Ok(Some(warning)) => {
                            diagnostic(&format!("OUTPUT WARNING: {warning}"));
                            let _ = tx.send(Msg::Log(format!("⚠ {warning}")));
                            let _ = tx.send(Msg::OutputWarning(warning));
                        }
                        Err(error) => {
                            diagnostic(&format!("OUTPUT INSPECTION ERROR: {error}"));
                            let _ = tx.send(Msg::Log(format!("⚠ 기존 출력 확인 실패: {error}")));
                        }
                        Ok(None) => {}
                    }
                }
            }
            // 대상 파일 목록을 미리 고정 (출력이 입력 하위에 있어도 무한 재귀 방지).
            let mut files = Vec::new();
            let mut failed = 0usize;
            for entry in WalkDir::new(&input).sort_by(|a, b| {
                natural_cmp(
                    &a.file_name().to_string_lossy(),
                    &b.file_name().to_string_lossy(),
                )
            }) {
                if control.cancelled() {
                    break;
                }
                match entry {
                    Ok(entry) if entry.file_type().is_file() && is_audio_file(entry.path()) => {
                        files.push(entry.into_path());
                    }
                    Err(error) => {
                        failed += 1;
                        diagnostic(&format!("SCAN ERROR: {error}"));
                        let _ = tx.send(Msg::Log(format!("⚠ 입력 탐색 실패: {error}")));
                    }
                    _ => {}
                }
            }

            let _ = tx.send(Msg::Total(files.len()));
            ctx2.request_repaint();

            let mut plans = Vec::new();
            for file in &files {
                if control.cancelled() {
                    break;
                }
                let track = if use_track_number && !has_track_prefix(file) {
                    match std::panic::catch_unwind(AssertUnwindSafe(|| read_track_number(file))) {
                        Ok(Ok(Some(number))) => Some(number),
                        other => {
                            let reason = match other {
                                Ok(Err(e)) => e.to_string(),
                                Err(_) => "메타데이터 내부 오류".to_owned(),
                                _ => "트랙 번호 태그 없음".to_owned(),
                            };
                            let _ = tx.send(Msg::Log(format!(
                                "⚠ 번호를 붙이지 않음: {} — {reason}",
                                file.display()
                            )));
                            ctx2.request_repaint();
                            None
                        }
                    }
                } else {
                    None
                };
                let relative =
                    numbered_wav_path(file.strip_prefix(&input).unwrap(), remove_artist, track);
                plans.push((file.clone(), relative));
            }
            plans.sort_by(|a, b| natural_cmp(&a.1.to_string_lossy(), &b.1.to_string_lossy()));

            let mut ok = 0usize;
            let mut completed_paths = Vec::new();

            for (i, (file, relative)) in plans.iter().enumerate() {
                if control.cancelled() {
                    break;
                }
                let rel = file
                    .strip_prefix(&input)
                    .unwrap_or(Path::new(""))
                    .display()
                    .to_string();

                let _ = tx.send(Msg::Progress {
                    done: i,
                    file: rel.clone(),
                });
                ctx2.request_repaint();

                // 파일 하나가 패닉을 일으켜도 전체 작업이 죽지 않도록 격리.
                let destination = if in_place {
                    &input
                } else {
                    output.as_ref().unwrap()
                }
                .join(relative);
                diagnostic(&format!(
                    "FILE START: {} -> {}",
                    file.display(),
                    destination.display()
                ));
                let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    if transaction.is_none() {
                        transaction = Some(OutputTransaction::new(root)?);
                    }
                    transaction
                        .as_mut()
                        .unwrap()
                        .convert(file, &destination, fmt, &control.cancel)
                }));

                match result {
                    Ok(Ok(())) => {
                        if in_place {
                            transaction.as_mut().unwrap().defer_source_removal(file);
                        }
                        completed_paths.push(relative.clone());
                        diagnostic(&format!(
                            "FILE OK: {} -> {}",
                            file.display(),
                            destination.display()
                        ));
                        ok += 1;
                        let _ = tx.send(Msg::Log(format!("✓ {rel} → {}", relative.display())));
                        #[cfg(test)]
                        if let Some(hook) = control.after_conversion.lock().unwrap().take() {
                            hook();
                        }
                    }
                    Ok(Err(e)) if is_cancelled(&e) => {
                        break;
                    }
                    Ok(Err(e)) => {
                        diagnostic(&format!("FILE ERROR: {} — {e:#}", file.display()));
                        failed += 1;
                        let _ = tx.send(Msg::Log(format!("⚠ 실패: {rel} — {e:#}")));
                    }
                    Err(_) => {
                        diagnostic(&format!("FILE PANIC: {}", file.display()));
                        failed += 1;
                        let _ = tx.send(Msg::Log(format!("⚠ 내부 오류로 건너뜀: {rel}")));
                    }
                }
                ctx2.request_repaint();
            }

            let mut ordering_error = None;
            if arrange && !completed_paths.is_empty() && !control.cancelled() {
                let root = if in_place {
                    &input
                } else {
                    output.as_ref().unwrap()
                };
                let destinations: Vec<_> = completed_paths
                    .iter()
                    .map(|relative| root.join(relative))
                    .collect();
                let _ = tx.send(Msg::Log("DAP 재생 순서 정리 중…".to_owned()));
                let _ = tx.send(Msg::Progress {
                    done: files.len(),
                    file: "DAP 재생 순서 정리".to_owned(),
                });
                ctx2.request_repaint();
                match arrange_output(root, &destinations) {
                    Ok(report) => {
                        let message = if report.physical_order_verified {
                            format!(
                                "✓ 저장 폴더 {}개에서 Windows가 읽은 목록이 번호순임을 확인: {}",
                                report.checked_directories.len(),
                                root.display()
                            )
                        } else {
                            format!(
                                "✓ 출력 폴더 재구성 완료: {} (DAP 재생 순서는 확인하지 않음)",
                                root.display()
                            )
                        };
                        diagnostic(&message);
                        let _ = tx.send(Msg::Log(message));
                        for (directory, entries) in report.checked_directories {
                            diagnostic(&format!(
                                "ORDER SNAPSHOT directory={} entries={entries:?}",
                                directory.display()
                            ));
                            let preview = entries
                                .iter()
                                .take(12)
                                .map(|name| name.to_string_lossy())
                                .collect::<Vec<_>>()
                                .join(" → ");
                            let _ = tx.send(Msg::Log(format!(
                                "목록 {}: {preview}{}",
                                directory.display(),
                                if entries.len() > 12 { " → …" } else { "" }
                            )));
                        }
                        if failed > 0 {
                            let _ = tx.send(Msg::Log(format!("⚠ 성공한 결과의 저장 폴더만 정리했습니다. 실패 {failed}개는 다시 변환해야 합니다.")));
                        }
                    }
                    Err(error) => {
                        diagnostic(&format!("ORDER ERROR: {error:#}"));
                        ordering_error = Some(format!("{error:#}"));
                        let _ = tx.send(Msg::OrderError(format!("{error:#}")));
                    }
                }
            }
            // Ordering completes its directory swap before rollback. Never stop
            // a filesystem rename halfway through; cancellation stays pending.
            if !control.begin_commit() {
                let mut cleanup_error = transaction.as_mut().and_then(|transaction| {
                    transaction
                        .rollback()
                        .err()
                        .map(|error| format!("{error:#}"))
                });
                // Re-inserting backups can change FAT directory order. Keep
                // restored songs numbered when DAP ordering was requested.
                if arrange && cleanup_error.is_none() && ordering_error.is_none() {
                    let restored: Vec<_> = completed_paths
                        .iter()
                        .map(|path| root.join(path))
                        .filter(|path| path.is_file())
                        .collect();
                    if !restored.is_empty() {
                        if let Err(error) = arrange_output(root, &restored) {
                            cleanup_error = Some(format!(
                                "기존 파일은 복구했지만 순서 정리에 실패했습니다: {error:#}"
                            ));
                        }
                    }
                }
                if let Some(error) = ordering_error {
                    let message =
                        format!("순서 정리 중 오류가 있었습니다. 복구 위치를 확인하세요: {error}");
                    cleanup_error = Some(match cleanup_error {
                        Some(cleanup) => format!("{cleanup}\n{message}"),
                        None => message,
                    });
                }
                diagnostic(&format!("JOB CANCELLED cleanup_error={cleanup_error:?}"));
                let _ = tx.send(Msg::Cancelled { cleanup_error });
                ctx2.request_repaint();
                return;
            }
            ctx2.request_repaint();
            if let Some(transaction) = &mut transaction {
                if let Err(error) = transaction.commit() {
                    let mut message = format!("완료 처리 중 오류: {error:#}");
                    if transaction.is_committed() {
                        failed += 1;
                    } else {
                        failed += ok.max(1);
                        ok = 0;
                        completed_paths.clear();
                        match transaction.rollback() {
                            Ok(()) => message.push_str("\n이번 작업의 변경 사항을 되돌렸습니다."),
                            Err(error) => message.push_str(&format!("\n복구 중 오류: {error:#}")),
                        }
                    }
                    diagnostic(&format!("COMMIT ERROR: {message}"));
                    let _ = tx.send(Msg::OutputWarning(message.clone()));
                    let _ = tx.send(Msg::Log(format!("⚠ {message}")));
                }
            }
            diagnostic(&format!("JOB FINISHED: ok={ok} failed={failed} effective_output={output:?} in_place={in_place}"));
            if let Some(warning) = filename_warning(&completed_paths) {
                for path in &completed_paths {
                    if !path.to_string_lossy().is_ascii() {
                        let _ = tx.send(Msg::Log(format!("⚠ 비영문 이름: {}", path.display())));
                    }
                }
                let _ = tx.send(Msg::NameWarning(warning));
            }
            let _ = tx.send(Msg::Finished { ok, failed });
            ctx2.request_repaint();
        });

        self.job = Some(Job {
            control,
            rx,
            total: 0,
            done: 0,
            current: String::new(),
            order_error: None,
        });
    }
}

fn outside_output_warning(selected: &Path, effective: &Path) -> std::io::Result<Option<String>> {
    if selected == effective {
        return Ok(None);
    }
    let mut names = Vec::new();
    for entry in std::fs::read_dir(selected)? {
        let entry = entry?;
        if entry.file_type()?.is_file() && is_audio_file(&entry.path()) {
            names.push(entry.file_name());
        }
    }
    if names.is_empty() {
        return Ok(None);
    }
    let preview = names
        .iter()
        .take(9)
        .map(|name| name.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" → ");
    Ok(Some(format!(
        "선택한 위치 {}에 기존 음원 {}개가 있습니다.\n이번 결과는 {}에 저장하며, 이 폴더 밖의 기존 음원은 정리 대상에 포함되지 않습니다.\nDAP에서 WAV 폴더를 열어 결과를 확인하세요.\n\n기존 목록: {preview}",
        selected.display(), names.len(), effective.display()
    )))
}

/// 큰 폴더 박스를 그린다.
/// 반환: (박스 사각형, 박스 영역 클릭됨, "폴더 선택" 버튼 눌림)
/// `highlight`이면(=드래그 중 커서가 이 박스 위) 파란 테두리로 강조.
fn drop_zone(
    ui: &mut egui::Ui,
    height: f32,
    title: &str,
    current: &Option<PathBuf>,
    hint: &str,
    enabled: bool,
    highlight: bool,
) -> (egui::Rect, bool, bool) {
    let size = egui::vec2(ui.available_width(), height);
    // 배경 영역을 먼저 할당(낮은 우선순위) → 위에 그릴 버튼이 클릭을 가져감.
    let (rect, bg) = ui.allocate_exact_size(size, egui::Sense::click());

    let (fill, stroke) = if highlight {
        (
            egui::Color32::from_rgb(30, 45, 65),
            egui::Stroke::new(2.5, egui::Color32::from_rgb(90, 170, 255)),
        )
    } else {
        (
            ui.visuals().extreme_bg_color,
            ui.visuals().widgets.noninteractive.bg_stroke,
        )
    };
    ui.painter().rect(rect, 6.0, fill, stroke);

    let mut pick = false;
    let mut content = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink(12.0))
            .layout(egui::Layout::top_down(egui::Align::Center)),
    );
    content.add_enabled_ui(enabled, |ui| {
        ui.add_space(6.0);
        ui.heading(title);
        if highlight {
            ui.colored_label(egui::Color32::from_rgb(90, 170, 255), "⬇ 여기에 놓기");
        }
        ui.add_space(8.0);
        if ui.button("📂 폴더 선택").clicked() {
            pick = true;
        }
        ui.add_space(10.0);
        match current {
            Some(p) => {
                ui.strong("선택됨:");
                ui.label(p.display().to_string());
            }
            None => {
                ui.weak(hint);
            }
        }
    });

    (rect, enabled && bg.clicked(), pick)
}

/// 드롭된 파일의 마우스 위치를 egui 좌표(points)로 조회.
/// Windows에서는 OS API(`GetCursorPos`+`ScreenToClient`)로 직접 얻는다.
/// (winit이 드래그 중 좌표를 제공하지 않기 때문)
#[cfg(windows)]
fn os_cursor_in_points(ctx: &egui::Context, frame: &eframe::Frame) -> Option<egui::Pos2> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Foundation::{HWND, POINT};
    use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;

    let hwnd: HWND = match frame.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => h.hwnd.get() as HWND,
        _ => return None,
    };

    let mut pt = POINT { x: 0, y: 0 };
    unsafe {
        if GetCursorPos(&mut pt) == 0 {
            return None;
        }
        if ScreenToClient(hwnd, &mut pt) == 0 {
            return None;
        }
    }
    let ppp = ctx.pixels_per_point();
    Some(egui::pos2(pt.x as f32 / ppp, pt.y as f32 / ppp))
}

/// 비 Windows에서는 위치를 못 얻으므로 None (클릭 선택으로 폴백).
#[cfg(not(windows))]
fn os_cursor_in_points(_ctx: &egui::Context, _frame: &eframe::Frame) -> Option<egui::Pos2> {
    None
}

/// 한글이 깨지지 않도록 시스템 한글 폰트를 egui에 등록.
fn install_korean_font(ctx: &egui::Context) {
    let candidates: &[&str] = &[
        // Windows
        r"C:\Windows\Fonts\malgun.ttf",
        r"C:\Windows\Fonts\malgunsl.ttf",
        r"C:\Windows\Fonts\gulim.ttc",
        // macOS (개발용)
        "/System/Library/Fonts/AppleSDGothicNeo.ttc",
        "/Library/Fonts/AppleGothic.ttf",
        // Linux (개발용)
        "/usr/share/fonts/truetype/nanum/NanumGothic.ttf",
    ];

    let font_data = candidates.iter().find_map(|p| std::fs::read(p).ok());
    let Some(bytes) = font_data else {
        return;
    };

    let mut fonts = egui::FontDefinitions::default();
    fonts
        .font_data
        .insert("korean".to_owned(), egui::FontData::from_owned(bytes));
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "korean".to_owned());
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .push("korean".to_owned());

    ctx.set_fonts(fonts);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_test_wav(path: &Path) {
        let mut writer = hound::WavWriter::create(
            path,
            hound::WavSpec {
                channels: 1,
                sample_rate: 44100,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for sample in 0..1000i16 {
            writer.write_sample(sample).unwrap();
        }
        writer.finalize().unwrap();
    }

    #[test]
    fn cancel_after_a_completed_track_rolls_back_entire_gui_job() {
        for (in_place, overwrite) in [(false, false), (false, true), (true, true)] {
            // An explicitly supplied test root exercises the same GUI worker
            // on FAT media; only this newly created private fixture is touched.
            let fixture = match std::env::var_os("WAV_CANCEL_TEST_ROOT") {
                Some(root) => tempfile::Builder::new()
                    .prefix("wav-cancel-test-")
                    .tempdir_in(root)
                    .unwrap(),
                None => tempfile::tempdir().unwrap(),
            };
            let input = fixture.path().join("input");
            let output = if in_place {
                input.clone()
            } else {
                fixture.path().join("output")
            };
            std::fs::create_dir(&input).unwrap();
            let source = input.join("01 Artist - Song.wav");
            write_test_wav(&source);
            write_test_wav(&input.join("02 Artist - Next.wav"));
            let original_source = std::fs::read(&source).unwrap();
            let destination = output.join("01 Song.wav");
            if overwrite {
                std::fs::create_dir_all(&output).unwrap();
                std::fs::write(&destination, b"previous output").unwrap();
                std::fs::write(output.join("keep.txt"), b"unrelated").unwrap();
                std::fs::write(output.join("03 Existing.wav"), b"third original").unwrap();
                std::fs::write(output.join("02 Existing.wav"), b"second original").unwrap();
            }
            let control = Arc::new(JobControl::default());
            let (ready_tx, ready_rx) = sync_channel(1);
            let (resume_tx, resume_rx) = sync_channel(1);
            *control.after_conversion.lock().unwrap() = Some(Box::new(move || {
                ready_tx.send(()).unwrap();
                resume_rx
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap();
            }));
            let mut app = App {
                input_dir: Some(input.clone()),
                output_dir: Some(output.clone()),
                same_as_input: in_place,
                remove_artist: true,
                ..Default::default()
            };
            let ctx = egui::Context::default();
            app.start_job_with_control(&ctx, Arc::clone(&control));
            ready_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
            assert!(hound::WavReader::open(&destination).is_ok());
            assert!(control.request_cancel());
            resume_tx.send(()).unwrap();
            wait_for_job(&mut app, &ctx);
            assert!(app.summary.as_deref().unwrap().starts_with("취소 완료"));
            assert!(app.output_warning.is_none());
            assert_eq!(std::fs::read(&source).unwrap(), original_source);
            assert!(!output.join("02 Next.wav").exists());
            if overwrite {
                assert_eq!(std::fs::read(destination).unwrap(), b"previous output");
                assert_eq!(
                    std::fs::read(output.join("keep.txt")).unwrap(),
                    b"unrelated"
                );
                assert_eq!(
                    std::fs::read(output.join("02 Existing.wav")).unwrap(),
                    b"second original"
                );
                assert_eq!(
                    std::fs::read(output.join("03 Existing.wav")).unwrap(),
                    b"third original"
                );
                let names: Vec<_> = std::fs::read_dir(&output)
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name())
                    .collect();
                let mut sorted = names.clone();
                sorted.sort_by(|a, b| natural_cmp(&a.to_string_lossy(), &b.to_string_lossy()));
                assert_eq!(
                    names, sorted,
                    "restored output must retain numbered directory order"
                );
            } else {
                assert!(
                    !output.exists(),
                    "newly created output folder must be removed"
                );
            }
            assert!(std::fs::read_dir(fixture.path()).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".wav-converter-job-")
            }));
        }
    }

    #[test]
    fn successful_in_place_gui_job_commits_results_and_removes_old_source() {
        let fixture = tempfile::tempdir().unwrap();
        let input = fixture.path().join("album");
        std::fs::create_dir(&input).unwrap();
        let source = input.join("01 Artist - Song.wav");
        write_test_wav(&source);
        let mut app = App {
            input_dir: Some(input.clone()),
            same_as_input: true,
            remove_artist: true,
            ..Default::default()
        };
        let ctx = egui::Context::default();
        app.start_job(&ctx);
        wait_for_job(&mut app, &ctx);
        assert!(app.summary.as_deref().unwrap().starts_with("✅"));
        assert!(!source.exists());
        assert_eq!(
            hound::WavReader::open(input.join("01 Song.wav"))
                .unwrap()
                .duration(),
            1000
        );
        assert_eq!(std::fs::read_dir(fixture.path()).unwrap().count(), 1);
    }

    #[test]
    fn cancellation_and_commit_are_mutually_exclusive() {
        let cancel_first = JobControl::default();
        assert!(cancel_first.request_cancel());
        assert!(!cancel_first.begin_commit());
        let commit_first = JobControl::default();
        assert!(commit_first.begin_commit());
        assert!(!commit_first.request_cancel());
        assert!(!commit_first.cancel.load(Ordering::SeqCst));
    }

    fn wait_for_job(app: &mut App, ctx: &egui::Context) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
        while app.job.is_some() {
            app.pump_messages(ctx);
            assert!(std::time::Instant::now() < deadline, "conversion timed out");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn failed_conversion_is_not_presented_as_complete_success() {
        let input = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        std::fs::write(input.path().join("01 broken.wav"), b"invalid WAV").unwrap();
        let mut app = App {
            input_dir: Some(input.path().to_owned()),
            output_dir: Some(output.path().to_owned()),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        app.start_job(&ctx);
        wait_for_job(&mut app, &ctx);
        assert!(app.summary.as_deref().unwrap().contains("일부 변환 실패"));
        assert!(app.output_warning.is_some());
        assert!(!app.log.iter().any(|line| line.starts_with("✓")));
        assert_eq!(std::fs::read_dir(output.path()).unwrap().count(), 0);
    }

    #[test]
    fn unreadable_input_is_reported_as_failure() {
        let fixture = tempfile::tempdir().unwrap();
        let mut app = App {
            input_dir: Some(fixture.path().join("missing input")),
            output_dir: Some(fixture.path().join("output")),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        app.start_job(&ctx);
        wait_for_job(&mut app, &ctx);
        assert!(app.summary.as_deref().unwrap().contains("실패 1개"));
        assert!(app.output_warning.is_some());
        assert!(app.log.iter().any(|line| line.contains("입력 탐색 실패")));
        assert!(!fixture.path().join("output").exists());
    }

    #[test]
    fn warning_distinguishes_existing_root_music_from_new_output() {
        let selected = tempfile::tempdir().unwrap();
        let effective = selected.path().join("WAV");
        assert!(outside_output_warning(selected.path(), &effective)
            .unwrap()
            .is_none());
        let existing = selected.path().join("07. Haruka.wav");
        std::fs::write(&existing, b"existing song").unwrap();
        let warning = outside_output_warning(selected.path(), &effective)
            .unwrap()
            .unwrap();
        assert!(warning.contains("기존 음원 1개"));
        assert!(warning.contains(&effective.display().to_string()));
        assert!(warning.contains("07. Haruka.wav"));
        assert_eq!(std::fs::read(existing).unwrap(), b"existing song");
    }

    /// Uses the same worker as the Start button. Only run with explicit paths;
    /// like a GUI conversion, results remain in the selected output folder.
    #[test]
    #[ignore = "requires WAV_REPRO_INPUT and WAV_REPRO_OUTPUT; writes converted results"]
    fn reproduce_gui_conversion_from_environment() {
        let input = PathBuf::from(std::env::var_os("WAV_REPRO_INPUT").expect("input folder"));
        let selected = PathBuf::from(std::env::var_os("WAV_REPRO_OUTPUT").expect("output folder"));
        let effective = effective_output_dir(&selected, true);
        let mut app = App {
            input_dir: Some(input),
            output_dir: Some(selected),
            format: Format(OutputFormat::Pcm16_44100),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        app.start_job(&ctx);
        wait_for_job(&mut app, &ctx);
        for line in &app.log {
            println!("{line}");
        }
        println!("{}", app.summary.as_deref().unwrap_or_default());
        assert!(app.summary.as_deref().unwrap().starts_with("✅"));
        assert!(effective.is_dir());
        assert!(app.log.iter().any(|line| line.starts_with("목록 ")));
    }

    #[test]
    fn filename_warning_survives_completion_and_renders() {
        let ctx = egui::Context::default();
        let mut app = App::default();
        let (tx, rx) = sync_channel(2);
        app.job = Some(Job {
            rx,
            total: 1,
            done: 0,
            current: String::new(),
            order_error: None,
            control: Arc::default(),
        });
        tx.send(Msg::NameWarning("01 日本語.wav".to_owned()))
            .unwrap();
        tx.send(Msg::Finished { ok: 1, failed: 0 }).unwrap();
        app.pump_messages(&ctx);
        assert!(app.job.is_none());
        assert_eq!(app.filename_warning.as_deref(), Some("01 日本語.wav"));
        for _ in 0..3 {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(760.0, 620.0),
                    )),
                    ..Default::default()
                },
                |ctx| app.draw_ui(ctx, false, false, false),
            );
        }
        assert!(app.filename_warning.is_some());
    }

    #[test]
    fn conversion_ui_stays_finite_while_pointer_moves() {
        let ctx = egui::Context::default();
        let (_tx, rx) = sync_channel(1);
        let mut app = App::default();
        app.job = Some(Job {
            rx,
            total: 28,
            done: 6,
            current: "07 Artist - Virtual Storm Hard Arrange.mp3".to_owned(),
            order_error: None,
            control: Arc::default(),
        });
        for i in 0..14 {
            app.log.push_back(format!("Converted track {i}"));
        }
        for frame in 0..120 {
            let width = if frame % 2 == 0 { 760.0 } else { 640.0 };
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(width, 620.0),
                )),
                events: vec![egui::Event::PointerMoved(egui::pos2(
                    40.0 + (frame % 10) as f32 * 55.0,
                    450.0 + (frame % 6) as f32 * 25.0,
                ))],
                ..Default::default()
            };
            let output = ctx.run(input, |ctx| app.draw_ui(ctx, false, false, false));
            for primitive in ctx.tessellate(output.shapes, output.pixels_per_point) {
                if let egui::epaint::Primitive::Mesh(mesh) = primitive.primitive {
                    assert!(
                        mesh.vertices
                            .iter()
                            .all(|v| v.pos.x.is_finite() && v.pos.y.is_finite()),
                        "non-finite UI geometry"
                    );
                }
            }
        }
    }

    #[test]
    fn many_results_keep_ui_log_bounded() {
        let mut app = App::default();
        let ctx = egui::Context::default();
        let (tx, rx) = sync_channel(256);
        app.job = Some(Job {
            rx,
            total: 2000,
            done: 0,
            current: String::new(),
            order_error: None,
            control: Arc::default(),
        });
        for i in 0..2000 {
            tx.send(Msg::Log(format!("file {i}"))).unwrap();
            app.pump_messages(&ctx);
        }
        tx.send(Msg::Finished {
            ok: 2000,
            failed: 0,
        })
        .unwrap();
        app.pump_messages(&ctx);
        assert_eq!(app.log.len(), MAX_UI_LOGS);
        assert!(app.job.is_none());
        assert!(app.summary.unwrap().contains("2000"));
    }

    #[test]
    fn disconnected_worker_reports_interruption() {
        let mut app = App::default();
        let (tx, rx) = sync_channel(1);
        app.job = Some(Job {
            rx,
            total: 1,
            done: 0,
            current: String::new(),
            order_error: None,
            control: Arc::default(),
        });
        drop(tx);
        app.pump_messages(&egui::Context::default());
        assert!(app.job.is_none());
        assert!(app.summary.unwrap().contains("중단"));
    }
}
