extern crate ffmpeg_next as ffmpeg;

use ffmpeg::format::Pixel;
use ffmpeg::frame::Video;

use sdl2::pixels::PixelFormatEnum;
use sdl2::video::Window;

use std::error::Error;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

mod audio;
mod player;
mod video;

use crate::player::Player;

static SC_WIDTH: AtomicU32 = AtomicU32::new(800);
static SC_HEIGHT: AtomicU32 = AtomicU32::new(600);

fn main() -> Result<(), Box<dyn Error>> {
    // 创建带缓冲的通道，避免阻塞
    let (frame_sender, frame_receiver) = mpsc::channel::<Video>();

    let path = "/Users/chinaxxren/Desktop/a.mp4";
    println!("开始播放视频: {}", path);

    let width = SC_WIDTH.load(Ordering::Relaxed);
    let height = SC_HEIGHT.load(Ordering::Relaxed);

    let sdl_context = sdl2::init()?;
    let video_subsystem = sdl_context.video()?;
    let window: Window = video_subsystem
        .window("Video Player", width, height)
        .position_centered() // 居中显示
        .resizable() // 设置窗口可变
        .build()?;

    // 保持对 Player 的引用
    let player = Arc::new(Mutex::new(Player::start(
        path.into(),
        {
            move |frame| {
                let new_frame = rescaler_for_frame(&frame);
                if let Err(e) = frame_sender.send(new_frame) {
                    eprintln!("发送帧失败: {}", e);
                }
            }
        },
        move |playing| {
            println!("播放状态: {}", playing);
        },
    )?));

    let mut canvas = window.into_canvas().build()?;
    let texture_creator = canvas.texture_creator();
    let mut tex = None;

    let mut event_pump = sdl_context.event_pump()?;
    let mut frame_count = 0;
    let mut last_fps_update = Instant::now();
    let mut last_frame_time = Instant::now();

    'running: loop {
        for event in event_pump.poll_iter() {
            match event {
                sdl2::event::Event::Window {
                    win_event: sdl2::event::WindowEvent::Resized(x, _y),
                    ..
                } => {
                    println!("窗口大小已更改为: {}x{}", x, _y);
                }
                sdl2::event::Event::Quit { .. } => {
                    println!("接收到退出事件");
                    break 'running;
                }
                sdl2::event::Event::KeyDown {
                    keycode: Some(sdl2::keyboard::Keycode::Space),
                    ..
                } => {
                    if let Ok(mut player) = player.lock() {
                        player.toggle_pause_playing();
                    }
                }
                _ => {}
            }
        }

        match frame_receiver.try_recv() {
            Ok(frame) => {
                last_frame_time = Instant::now();
                frame_count += 1;

                if tex.is_none() {
                    println!("创建纹理: {}x{}", frame.width(), frame.height());
                    tex = Some(texture_creator.create_texture_streaming(
                        PixelFormatEnum::IYUV,
                        frame.width(),
                        frame.height(),
                    )?);
                }

                if let Some(ref mut tex) = tex {
                    tex.update_yuv(
                        None,
                        frame.data(0),
                        frame.stride(0),
                        frame.data(1),
                        frame.stride(1),
                        frame.data(2),
                        frame.stride(2),
                    )?;

                    canvas.clear();
                    canvas.copy(tex, None, None)?;
                    canvas.present();
                }

                if last_fps_update.elapsed() >= Duration::from_secs(1) {
                    println!("FPS: {}", frame_count);
                    frame_count = 0;
                    last_fps_update = Instant::now();
                }
            }
            Err(mpsc::TryRecvError::Empty) => {
                if last_frame_time.elapsed() > Duration::from_secs(5) {
                    println!("警告: 5秒未收到新帧");
                    // 检查 player 状态
                    if let Ok(_) = player.lock() {
                        println!("Player 仍然存在且可访问");
                    }
                    last_frame_time = Instant::now();
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                println!("播放器断开连接，退出循环");
                break 'running;
            }
        }
    }

    println!("主循环结束，开始清理资源");
    // 显式释放 player，确保资源正确清理
    drop(player);
    println!("资源清理完成");

    Ok(())
}

// 将视频帧缩放到指定的大小和格式转化为 YUV420P 格式
fn rescaler_for_frame(frame: &Video) -> Video {
    let width = SC_WIDTH.load(Ordering::Relaxed);
    let height = SC_HEIGHT.load(Ordering::Relaxed);
    println!("screen size: {}x{}", width, height);

    let mut context = ffmpeg_next::software::scaling::Context::get(
        frame.format(),
        frame.width(),
        frame.height(),
        Pixel::YUV420P, // Keep YUV420P format
        width,
        height,
        ffmpeg::software::scaling::Flags::BILINEAR,
    )
    .unwrap();

    let mut new_frame = Video::empty();
    context.run(&frame, &mut new_frame).unwrap();

    new_frame
}
