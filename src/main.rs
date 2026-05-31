// 릴리스 빌드에서는 콘솔 창이 같이 뜨지 않도록 함 (Windows).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::thread;

use wav_converter::convert::{convert_file, is_audio_file, OutputFormat};
use eframe::egui;
use walkdir::WalkDir;

fn main() -> eframe::Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([640.0, 540.0])
            .with_min_inner_size([520.0, 440.0]),
        ..Default::default()
    };

    eframe::run_native(
        "WAV 일괄 변환기",
        native_options,
        Box::new(|cc| {
            install_korean_font(&cc.egui_ctx);
            Ok(Box::new(App::default()))
        }),
    )
}

/// 백그라운드 변환 스레드 → UI로 보내는 메시지.
enum Msg {
    Total(usize),
    Progress { done: usize, file: String },
    Log(String),
    Finished { ok: usize, failed: usize },
}

/// 진행 중인 변환 작업 상태.
struct Job {
    rx: Receiver<Msg>,
    total: usize,
    done: usize,
    current: String,
}

#[derive(Default)]
struct App {
    input_dir: Option<PathBuf>,
    output_dir: Option<PathBuf>,
    format: Format,
    job: Option<Job>,
    log: Vec<String>,
    summary: Option<String>,
}

/// OutputFormat 기본값 래퍼 (Default 구현용).
struct Format(OutputFormat);
impl Default for Format {
    fn default() -> Self {
        Format(OutputFormat::Preserve)
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_dropped_files(ctx);
        self.pump_messages(ctx);

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(8.0);
            ui.heading("🎵 WAV 일괄 변환기");
            ui.label("폴더 안의 모든 음원을 WAV로 변환합니다. (하위 폴더 구조 그대로 복제)");
            ui.add_space(12.0);

            let running = self.job.is_some();

            // 입력 폴더
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(!running, |ui| {
                        if ui.button("📂 입력 폴더 선택").clicked() {
                            if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                                self.input_dir = Some(dir);
                            }
                        }
                    });
                    ui.label(path_text(&self.input_dir));
                });
                ui.small("팁: 폴더를 이 창에 끌어다 놓아도 됩니다.");
            });

            ui.add_space(6.0);

            // 출력 폴더
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(!running, |ui| {
                        if ui.button("💾 출력 폴더 선택").clicked() {
                            if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                                self.output_dir = Some(dir);
                            }
                        }
                    });
                    ui.label(path_text(&self.output_dir));
                });
            });

            ui.add_space(6.0);

            // 출력 규격
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

            // 변환 시작
            let can_start =
                !running && self.input_dir.is_some() && self.output_dir.is_some();
            ui.add_enabled_ui(can_start, |ui| {
                if ui
                    .add(egui::Button::new("▶  변환 시작").min_size(egui::vec2(140.0, 32.0)))
                    .clicked()
                {
                    self.start_job(ctx);
                }
            });

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(6.0);

            // 진행 상황
            if let Some(job) = &self.job {
                let frac = if job.total == 0 {
                    0.0
                } else {
                    job.done as f32 / job.total as f32
                };
                ui.add(
                    egui::ProgressBar::new(frac)
                        .text(format!("{} / {}", job.done, job.total))
                        .desired_width(f32::INFINITY),
                );
                ui.label(format!("변환 중: {}", job.current));
            } else if let Some(summary) = &self.summary {
                ui.label(summary);
            }

            ui.add_space(8.0);

            // 로그
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for line in &self.log {
                        ui.monospace(line);
                    }
                });
        });
    }
}

impl App {
    /// 드래그&드롭으로 들어온 폴더를 입력 폴더로 설정.
    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        if self.job.is_some() {
            return;
        }
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        for f in dropped {
            if let Some(path) = f.path {
                if path.is_dir() {
                    self.input_dir = Some(path);
                } else if let Some(parent) = path.parent() {
                    // 파일을 떨어뜨리면 그 파일이 든 폴더를 입력으로.
                    self.input_dir = Some(parent.to_path_buf());
                }
                break;
            }
        }
    }

    /// 백그라운드 스레드에서 온 메시지 처리.
    fn pump_messages(&mut self, ctx: &egui::Context) {
        let mut finished = false;
        if let Some(job) = &mut self.job {
            // 채널의 모든 대기 메시지를 비운다.
            loop {
                match job.rx.try_recv() {
                    Ok(Msg::Total(n)) => job.total = n,
                    Ok(Msg::Progress { done, file }) => {
                        job.done = done;
                        job.current = file;
                    }
                    Ok(Msg::Log(line)) => self.log.push(line),
                    Ok(Msg::Finished { ok, failed }) => {
                        self.summary = Some(format!(
                            "✅ 완료 — 성공 {ok}개, 실패 {failed}개"
                        ));
                        self.log.push(format!(
                            "── 작업 완료: 성공 {ok}, 실패 {failed} ──"
                        ));
                        finished = true;
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        finished = true;
                        break;
                    }
                }
            }
            // 작업 중에는 계속 다시 그린다.
            ctx.request_repaint();
        }
        if finished {
            self.job = None;
        }
    }

    /// 변환 작업 시작 (파일 목록을 먼저 수집 후 스레드 실행).
    fn start_job(&mut self, ctx: &egui::Context) {
        let input = self.input_dir.clone().unwrap();
        let output = self.output_dir.clone().unwrap();
        let fmt = self.format.0;

        self.log.clear();
        self.summary = None;
        self.log
            .push(format!("입력: {}", input.display()));
        self.log
            .push(format!("출력: {}", output.display()));

        let (tx, rx) = channel::<Msg>();
        let ctx2 = ctx.clone();

        thread::spawn(move || {
            // 대상 파일 목록을 미리 고정 (출력이 입력 하위에 있어도 무한 재귀 방지).
            let files: Vec<PathBuf> = WalkDir::new(&input)
                .into_iter()
                .filter_map(Result::ok)
                .filter(|e| e.file_type().is_file())
                .map(|e| e.into_path())
                .filter(|p| is_audio_file(p))
                .collect();

            let _ = tx.send(Msg::Total(files.len()));
            ctx2.request_repaint();

            let mut ok = 0usize;
            let mut failed = 0usize;

            for (i, file) in files.iter().enumerate() {
                let rel = file.strip_prefix(&input).unwrap_or(Path::new(""));
                let out_path = output.join(rel).with_extension("wav");

                let _ = tx.send(Msg::Progress {
                    done: i,
                    file: rel.display().to_string(),
                });
                ctx2.request_repaint();

                match convert_file(file, &out_path, fmt) {
                    Ok(()) => {
                        ok += 1;
                        let _ = tx.send(Msg::Log(format!("✓ {}", rel.display())));
                    }
                    Err(e) => {
                        failed += 1;
                        let _ = tx.send(Msg::Log(format!(
                            "⚠ 실패: {} — {:#}",
                            rel.display(),
                            e
                        )));
                    }
                }
                ctx2.request_repaint();
            }

            let _ = tx.send(Msg::Finished { ok, failed });
            ctx2.request_repaint();
        });

        self.job = Some(Job {
            rx,
            total: 0,
            done: 0,
            current: String::new(),
        });
    }
}

/// 경로를 보기 좋은 문자열로.
fn path_text(p: &Option<PathBuf>) -> String {
    match p {
        Some(p) => p.display().to_string(),
        None => "(선택 안 됨)".to_string(),
    }
}

/// 한글이 깨지지 않도록 시스템 한글 폰트를 egui에 등록.
fn install_korean_font(ctx: &egui::Context) {
    // OS별 후보 한글 폰트 경로.
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

    let font_data = candidates
        .iter()
        .find_map(|p| std::fs::read(p).ok());

    let Some(bytes) = font_data else {
        // 한글 폰트를 못 찾아도 앱은 동작 (영문/숫자는 정상).
        return;
    };

    let mut fonts = egui::FontDefinitions::default();
    fonts
        .font_data
        .insert("korean".to_owned(), egui::FontData::from_owned(bytes));

    // 모든 패밀리의 맨 앞에 추가 → 한글 우선 렌더링.
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
