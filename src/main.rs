// 릴리스 빌드에서는 콘솔 창이 같이 뜨지 않도록 함 (Windows).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::thread;

use eframe::egui;
use wav_converter::convert::{convert_file, convert_in_place, is_audio_file, OutputFormat};
use walkdir::WalkDir;

fn main() -> eframe::Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([760.0, 620.0])
            .with_min_inner_size([640.0, 520.0]),
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
    /// 출력을 입력과 동일하게 (원본을 WAV로 대체).
    same_as_input: bool,
    job: Option<Job>,
    log: Vec<String>,
    summary: Option<String>,
    // 드롭 영역 판정을 위한 직전 프레임 사각형.
    input_rect: Option<egui::Rect>,
    output_rect: Option<egui::Rect>,
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

        let running = self.job.is_some();
        let hovering_files = ctx.input(|i| !i.raw.hovered_files.is_empty());
        let pointer = ctx.input(|i| i.pointer.hover_pos());

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(8.0);
            ui.heading("🎵 WAV 일괄 변환기");
            ui.label("폴더 안의 모든 음원을 WAV로 변환합니다. (하위 폴더 구조 그대로 복제)");
            ui.add_space(10.0);

            // ===== 좌우 큰 드롭 영역 =====
            let zone_height = 150.0;
            ui.columns(2, |cols| {
                // --- 왼쪽: 입력 폴더 ---
                let resp = drop_zone(
                    &mut cols[0],
                    zone_height,
                    "📥 입력 폴더",
                    &self.input_dir,
                    "여기로 폴더를 끌어다 놓으세요\n(또는 아래 버튼)",
                    !running,
                    hovering_files,
                    pointer,
                    self.input_rect,
                );
                if resp.select_clicked {
                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        self.input_dir = Some(dir);
                    }
                }
                self.input_rect = Some(resp.rect);

                // --- 오른쪽: 출력 폴더 ---
                let out_enabled = !running && !self.same_as_input;
                let out_hint = if self.same_as_input {
                    "원본 대체 모드 — 출력 폴더 사용 안 함"
                } else {
                    "여기로 폴더를 끌어다 놓으세요\n(또는 아래 버튼)"
                };
                let resp = drop_zone(
                    &mut cols[1],
                    zone_height,
                    "💾 출력 폴더",
                    &self.output_dir,
                    out_hint,
                    out_enabled,
                    hovering_files && !self.same_as_input,
                    pointer,
                    self.output_rect,
                );
                if resp.select_clicked {
                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        self.output_dir = Some(dir);
                    }
                }
                self.output_rect = Some(resp.rect);
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
    }
}

impl App {
    /// 드래그&드롭으로 들어온 폴더를 — 떨어뜨린 위치에 따라 — 입력/출력으로 설정.
    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
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

        // 떨어뜨린 위치로 영역 판정.
        let pos = ctx.input(|i| i.pointer.interact_pos().or(i.pointer.latest_pos()));
        let on_output = match (pos, self.output_rect) {
            (Some(p), Some(r)) => r.contains(p),
            _ => false,
        };
        let on_input = match (pos, self.input_rect) {
            (Some(p), Some(r)) => r.contains(p),
            _ => false,
        };

        if on_output && !self.same_as_input {
            self.output_dir = Some(folder);
        } else if on_input {
            self.input_dir = Some(folder);
        } else {
            // 영역 밖이면 입력으로 기본 처리.
            self.input_dir = Some(folder);
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
                    Ok(Msg::Log(line)) => self.log.push(line),
                    Ok(Msg::Finished { ok, failed }) => {
                        self.summary =
                            Some(format!("✅ 완료 — 성공 {ok}개, 실패 {failed}개"));
                        self.log
                            .push(format!("── 작업 완료: 성공 {ok}, 실패 {failed} ──"));
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
            ctx.request_repaint();
        }
        if finished {
            self.job = None;
        }
    }

    /// 변환 작업 시작 (파일 목록을 먼저 수집 후 스레드 실행).
    fn start_job(&mut self, ctx: &egui::Context) {
        let input = self.input_dir.clone().unwrap();
        let output = self.output_dir.clone();
        let fmt = self.format.0;
        let in_place = self.same_as_input;

        self.log.clear();
        self.summary = None;
        self.log.push(format!("입력: {}", input.display()));
        if in_place {
            self.log.push("모드: 원본 대체 (in-place)".to_string());
        } else if let Some(o) = &output {
            self.log.push(format!("출력: {}", o.display()));
        }

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
                let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    if in_place {
                        convert_in_place(file, fmt)
                    } else {
                        let out_path = output
                            .as_ref()
                            .unwrap()
                            .join(file.strip_prefix(&input).unwrap_or(Path::new("")))
                            .with_extension("wav");
                        convert_file(file, &out_path, fmt)
                    }
                }));

                match result {
                    Ok(Ok(())) => {
                        ok += 1;
                        let _ = tx.send(Msg::Log(format!("✓ {rel}")));
                    }
                    Ok(Err(e)) => {
                        failed += 1;
                        let _ = tx.send(Msg::Log(format!("⚠ 실패: {rel} — {e:#}")));
                    }
                    Err(_) => {
                        failed += 1;
                        let _ = tx.send(Msg::Log(format!("⚠ 내부 오류로 건너뜀: {rel}")));
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

/// 드롭존 위젯의 반환값.
struct ZoneResp {
    rect: egui::Rect,
    select_clicked: bool,
}

/// 큰 드롭 영역 + 폴더 선택 버튼을 그린다.
#[allow(clippy::too_many_arguments)]
fn drop_zone(
    ui: &mut egui::Ui,
    height: f32,
    title: &str,
    current: &Option<PathBuf>,
    hint: &str,
    enabled: bool,
    hovering_files: bool,
    pointer: Option<egui::Pos2>,
    prev_rect: Option<egui::Rect>,
) -> ZoneResp {
    // 이 영역 위로 파일을 드래그 중이면 강조.
    let highlighted = enabled
        && hovering_files
        && match (pointer, prev_rect) {
            (Some(p), Some(r)) => r.contains(p),
            _ => false,
        };

    let mut select_clicked = false;

    let frame = egui::Frame::group(ui.style())
        .stroke(if highlighted {
            egui::Stroke::new(2.5, egui::Color32::from_rgb(90, 170, 255))
        } else {
            ui.visuals().widgets.noninteractive.bg_stroke
        })
        .fill(if highlighted {
            egui::Color32::from_rgb(30, 45, 65)
        } else {
            ui.visuals().extreme_bg_color
        });

    let inner = frame.show(ui, |ui| {
        ui.set_min_height(height);
        ui.set_min_width(ui.available_width());
        ui.add_enabled_ui(enabled, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(8.0);
                ui.heading(title);
                ui.add_space(10.0);
                if ui.button("📂 폴더 선택").clicked() {
                    select_clicked = true;
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
        });
    });

    ZoneResp {
        rect: inner.response.rect,
        select_clicked,
    }
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
