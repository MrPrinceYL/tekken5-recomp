//! Bevy host for the recompiled Tekken 5 (SLUS-21059).
//!
//! The recompiled game runs on its own thread against the emulated PlayStation 2 (ps2-runtime).
//! Every vertical blank the GS's display output is published as an RGBA frame; this app uploads
//! it into a texture on a 4:3 quad in a 3D scene seen by an orthographic camera, plays the SPU2's
//! output through the default audio device and feeds keyboard / gamepad input into the emulated
//! controller ports.
//!
//! Keys: F1 debug overlay, F2 controls, P settings, F11 / Alt+Enter fullscreen.

mod hid;
mod input;
mod settings;

use bevy::asset::RenderAssetUsages;
use bevy::camera::ScalingMode;
use bevy::core_pipeline::tonemapping::Tonemapping;
use bevy::image::ImageSampler;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use ps2_runtime::gs::{Frame, FrameShared};
use ps2_runtime::spu2::AudioShared;
use ps2_runtime::{CALLS, FN_ENTRIES, LAST_FN, UNKNOWN_TARGETS};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;

#[derive(Resource)]
struct Screen {
    frames: Arc<FrameShared>,
    image: Handle<Image>,
    material: Handle<StandardMaterial>,
    seen: u64,
    buf: Frame,
    size: (u32, u32),
    smooth: bool,
    gamma: f32,
    lut: [u8; 256],
}

#[derive(Resource)]
struct PendingLink(Option<Arc<FrameShared>>);

#[derive(Component)]
struct StatusText;

/// Devil Within's debug menus (T5_DW_DEBUG), whose own text output the retail build removed.
#[derive(Component)]
struct DebugMenuText;

fn main() {
    let shared = t5_recomp::boot();
    std::thread::Builder::new()
        .name("ps2-ee".into())
        .stack_size(512 << 20)
        .spawn(|| {
            #[cfg(windows)]
            t5_recomp::profile::mark_guest();
            t5_recomp::run()
        })
        .expect("spawn guest thread");
    // T5_PROFILE=<second>: sample a thread (T5_PROFILE_MTVU / _GS / _RASTER, else the guest)
    // for T5_PROFILE_SECS (default 30) seconds from that game second on; result in t5_trace.log.
    #[cfg(windows)]
    if let Some(start) = std::env::var("T5_PROFILE").ok().and_then(|v| v.parse::<f64>().ok()) {
        let span = std::env::var("T5_PROFILE_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(30);
        std::thread::spawn(move || {
            while ps2_runtime::secs() < start {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            ps2_runtime::diag::log(format!("profile: sampling {span} s"));
            t5_recomp::profile::report(t5_recomp::profile::run(span));
        });
    }
    // Measurement runs: `--press BUTTON@SECOND` as in t5-headless.
    t5_recomp::script::init(&std::env::args().collect::<Vec<_>>());
    start_audio(shared.audio.clone());

    let mut app = App::new();
    app.insert_resource(ClearColor(Color::BLACK)).add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: "Tekken 5 (Tekken5Decomp)".into(),
            resolution: (1280, 960).into(),
            ..default()
        }),
        ..default()
    }));
    app.insert_resource(PendingLink(Some(shared.frames.clone())))
        .insert_resource(input::Input::new(shared.pad.clone()))
        .insert_resource(settings::Settings::load())
        .init_resource::<settings::MenuState>()
        .add_systems(Startup, (setup, input::setup_menu, settings::setup, settings::setup_fps))
        .add_systems(Update, (show_frame, input::read_input, input::menu, settings::menu, fps_counter, update_status, toggle_overlay, fast_forward, update_debug_menu))
        .run();
}

/// Play the SPU2 output (48 kHz stereo) through the default device, resampled to its rate.
fn start_audio(src: Arc<AudioShared>) {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    let result = (|| -> Result<(), String> {
        let host = cpal::default_host();
        let dev = host.default_output_device().ok_or("no audio output device")?;
        let cfg = dev.default_output_config().map_err(|e| e.to_string())?;
        let channels = cfg.channels() as usize;
        let out_rate = cfg.sample_rate() as f64;
        let config: cpal::StreamConfig = cfg.into();
        let mut pos = 0f64;
        let mut cur = (0f32, 0f32);
        let mut next = (0f32, 0f32);
        let mut playing = false;
        let mut rate_adj = 1f64;
        // The SPU2 output arrives in bursts (whenever the EE services devices): play from a
        // ~60 ms cushion, steer its fill level with a slight resampling rate change instead of
        // dropping samples, and refill after an underrun instead of repeating the last sample.
        let target = (ps2_runtime::spu2::SAMPLE_RATE as usize * 60 / 1000) * 2;
        let stream = dev
            .build_output_stream(
                &config,
                move |data: &mut [f32], _| {
                    let mut q = src.samples.lock().unwrap();
                    while q.len() > 6 * target {
                        q.pop_front();
                    }
                    if !playing && q.len() >= target {
                        playing = true;
                    }
                    let fill = q.len() as f64 / target as f64;
                    let want = 1.0 + ((fill - 1.0) * 0.02).clamp(-0.02, 0.02);
                    rate_adj += (want - rate_adj) * 0.05;
                    let step = ps2_runtime::spu2::SAMPLE_RATE as f64 / out_rate * rate_adj;
                    let vol = f32::from_bits(settings::VOLUME.load(Relaxed));
                    for frame in data.chunks_mut(channels) {
                        pos += step;
                        while pos >= 1.0 {
                            pos -= 1.0;
                            cur = next;
                            if playing && q.len() >= 2 {
                                next = (q.pop_front().unwrap() as f32, q.pop_front().unwrap() as f32);
                            } else {
                                // Underrun: fade to silence, wait for the cushion to refill.
                                playing = false;
                                next = (next.0 * 0.95, next.1 * 0.95);
                            }
                        }
                        let t = pos as f32;
                        let l = (cur.0 * (1.0 - t) + next.0 * t) / 32768.0 * vol;
                        let r = (cur.1 * (1.0 - t) + next.1 * t) / 32768.0 * vol;
                        for (i, s) in frame.iter_mut().enumerate() {
                            *s = if i % 2 == 0 { l } else { r };
                        }
                    }
                },
                |e| eprintln!("[t5] audio stream error: {e}"),
                None,
            )
            .map_err(|e| e.to_string())?;
        stream.play().map_err(|e| e.to_string())?;
        std::mem::forget(stream);
        Ok(())
    })();
    if let Err(e) = result {
        ps2_runtime::diag::log(format!("audio output unavailable: {e}"));
    }
}

fn new_image(w: u32, h: u32, smooth: bool) -> Image {
    let mut img = Image::new_fill(
        Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        TextureDimension::D2,
        &[0, 0, 0, 255],
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::default(),
    );
    img.sampler = if smooth { ImageSampler::linear() } else { ImageSampler::nearest() };
    img
}

fn setup(
    mut commands: Commands,
    mut pending: ResMut<PendingLink>,
    mut images: ResMut<Assets<Image>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    settings: Res<settings::Settings>,
) {
    let frames = pending.0.take().expect("game link");
    let image = images.add(new_image(640, 448, settings.smooth));
    let material = materials.add(StandardMaterial { base_color_texture: Some(image.clone()), unlit: true, ..default() });
    commands.spawn((
        Camera3d::default(),
        Projection::Orthographic(OrthographicProjection {
            scaling_mode: ScalingMode::AutoMin { min_width: 4.0, min_height: 3.0 },
            ..OrthographicProjection::default_3d()
        }),
        Tonemapping::None,
        Transform::from_xyz(0.0, 0.0, 10.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
    commands.spawn((Mesh3d(meshes.add(Rectangle::new(4.0, 3.0))), MeshMaterial3d(material.clone()), Transform::IDENTITY));
    commands.spawn((
        Text::new(""),
        TextFont { font_size: FontSize::Px(14.0), ..default() },
        TextColor(Color::srgb(0.9, 1.0, 0.6)),
        Node { position_type: PositionType::Absolute, top: px(8), left: px(8), ..default() },
        Visibility::Hidden,
        StatusText,
    ));
    commands.spawn((
        Text::new(""),
        TextFont { font_size: FontSize::Px(20.0), ..default() },
        TextColor(Color::WHITE),
        BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.7)),
        Node { position_type: PositionType::Absolute, top: px(40), left: px(40), padding: UiRect::all(px(10)), ..default() },
        Visibility::Hidden,
        DebugMenuText,
    ));
    commands.insert_resource(Screen {
        frames,
        image,
        material,
        seen: 0,
        buf: Frame::default(),
        size: (640, 448),
        smooth: settings.smooth,
        gamma: f32::NAN,
        lut: [0; 256],
    });
}

fn show_frame(
    mut screen: ResMut<Screen>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    settings: Res<settings::Settings>,
) {
    let s = &mut *screen;
    if s.gamma != settings.gamma {
        s.gamma = settings.gamma;
        for (i, v) in s.lut.iter_mut().enumerate() {
            *v = ((i as f32 / 255.0).powf(1.0 / settings.gamma) * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }
    let n = s.frames.take_if_new(s.seen, &mut s.buf);
    let restyle = s.smooth != settings.smooth;
    if n == s.seen && !restyle {
        return;
    }
    s.seen = n;
    s.smooth = settings.smooth;
    let (w, h) = (s.buf.width.max(1), s.buf.height.max(1));
    if s.buf.rgba.len() != (w * h * 4) as usize {
        return;
    }
    if s.size != (w, h) || restyle {
        let old = std::mem::replace(&mut s.image, images.add(new_image(w, h, s.smooth)));
        images.remove(&old);
        if let Some(mut mat) = materials.get_mut(&s.material) {
            mat.base_color_texture = Some(s.image.clone());
        }
        s.size = (w, h);
    }
    let Some(mut img) = images.get_mut(&s.image) else { return };
    if let Some(data) = img.data.as_mut() {
        if s.gamma == 1.0 {
            data.copy_from_slice(&s.buf.rgba);
        } else {
            for (d, v) in data.iter_mut().zip(s.buf.rgba.iter()) {
                *d = s.lut[*v as usize];
            }
            for px in data.chunks_exact_mut(4) {
                px[3] = 255;
            }
        }
    }
}

fn fps_counter(
    screen: Option<Res<Screen>>,
    settings: Res<settings::Settings>,
    mut w: Local<settings::FpsWindow>,
    mut q: Query<(&mut Text, &mut Visibility), With<settings::FpsText>>,
) {
    let Some(screen) = screen else { return };
    // New pictures, not fields: Tekken 5 shows an unchanged frame again each field.
    // Real game frames (copy passes from a newly drawn frame), not displayed pictures: the game
    // re-copies its frame every field with an interlace shift.
    // The game's own logic frame counter: displayed pictures and copy passes repeat every
    // field and would always read 60.
    let logic = ps2_runtime::machine_opt().map(|m| ps2_runtime::logic_frames(m) as u64).unwrap_or(0);
    let rendered = ps2_runtime::raster::RENDERED_TOTAL.load(Relaxed);
    settings::update_fps(rendered, logic, screen.frames.flips(), &settings, &mut w, &mut q);
}

fn update_status(screen: Option<Res<Screen>>, mut q: Query<(&mut Text, &Visibility), With<StatusText>>) {
    if q.iter().all(|(_, v)| *v == Visibility::Hidden) {
        return;
    }
    let Some(screen) = screen else { return };
    let last = LAST_FN.load(Relaxed);
    let s = format!(
        "Tekken 5 recomp | frames {} ({}x{})\nfunctions entered {} | indirect calls {} (unresolved {})\nlast function {:#010x}\nvblanks {}\n",
        screen.frames.count(),
        screen.size.0,
        screen.size.1,
        FN_ENTRIES.load(Relaxed),
        CALLS.load(Relaxed),
        UNKNOWN_TARGETS.load(Relaxed),
        last,
        ps2_runtime::VBLANKS.load(Relaxed),
    );
    for (mut t, _) in &mut q {
        t.0 = s.clone();
    }
}

/// Show the open Devil Within debug menus (t5_recomp::devil_debug::MENU_TEXT).
fn update_debug_menu(mut q: Query<(&mut Text, &mut Visibility), With<DebugMenuText>>) {
    let Ok(text) = t5_recomp::devil_debug::MENU_TEXT.lock().map(|t| t.clone()) else { return };
    for (mut t, mut v) in &mut q {
        let want = if text.is_empty() { Visibility::Hidden } else { Visibility::Visible };
        if *v != want {
            *v = want;
        }
        if t.0 != text {
            t.0 = text.clone();
        }
    }
}

/// F3 held: skip the idle time between frames (fast forward, `ps2_runtime::WARP_KEY`).
fn fast_forward(keys: Res<ButtonInput<KeyCode>>) {
    ps2_runtime::WARP_KEY.store(keys.pressed(KeyCode::F3), Relaxed);
}

fn toggle_overlay(keys: Res<ButtonInput<KeyCode>>, mut q: Query<&mut Visibility, With<StatusText>>) {
    if keys.just_pressed(KeyCode::F1) {
        for mut v in &mut q {
            v.toggle_visible_hidden();
        }
    }
}
