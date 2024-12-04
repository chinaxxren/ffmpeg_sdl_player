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

    // 保存当前窗口大小和显示区域
    let mut window_state = WindowState {
        size: (width, height),
        display_rect: None,
    };

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
                    win_event,
                    ..
                } => match win_event {
                    sdl2::event::WindowEvent::Resized(x, y) |
                    sdl2::event::WindowEvent::SizeChanged(x, y) => {
                        handle_window_resize(&mut window_state, x as u32, y as u32);
                    }
                    _ => {}
                },
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
                    
                    let video_width = frame.width();
                    let video_height = frame.height();
                    
                    // 只在需要时计算显示区域
                    if window_state.display_rect.is_none() {
                        let (window_width, window_height) = canvas.output_size()?;
                        window_state.update_display_rect(
                            window_width,
                            window_height,
                            video_width,
                            video_height
                        );
                    }
                    
                    let (x, y, w, h) = window_state.display_rect.unwrap();
                    let src_rect = sdl2::rect::Rect::new(0, 0, video_width, video_height);
                    let dst_rect = sdl2::rect::Rect::new(x, y, w, h);
                    
                    canvas.copy(tex, Some(src_rect), Some(dst_rect))?;
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

// 窗口状态结构体
struct WindowState {
    size: (u32, u32),
    display_rect: Option<(i32, i32, u32, u32)>,
}

impl WindowState {
    // 处理窗口大小调整
    fn handle_resize(&mut self, new_width: u32, new_height: u32) {
        let new_size = (new_width, new_height);
        // 只在窗口大小真正改变时更新
        if new_size != self.size {
            println!("窗口大小已更改为: {}x{}", new_width, new_height);
            self.size = new_size;
            // 清除缓存的显示区域，这样会在下次渲染时重新计算
            self.display_rect = None;
            // 更新全局状态
            SC_WIDTH.store(new_width, Ordering::Relaxed);
            SC_HEIGHT.store(new_height, Ordering::Relaxed);
        }
    }

    // 更新显示区域
    fn update_display_rect(&mut self, window_width: u32, window_height: u32, video_width: u32, video_height: u32) {
        self.display_rect = Some(calculate_display_rect(
            window_width,
            window_height,
            video_width,
            video_height
        ));
    }
}

// 处理窗口调整事件
fn handle_window_resize(window_state: &mut WindowState, width: u32, height: u32) {
    window_state.handle_resize(width, height);
}

// 计算保持宽高比的显示区域
fn calculate_display_rect(
    window_width: u32,
    window_height: u32,
    video_width: u32,
    video_height: u32,
) -> (i32, i32, u32, u32) {
    let window_aspect = window_width as f32 / window_height as f32;
    let video_aspect = video_width as f32 / video_height as f32;
    
    let (w, h) = if window_aspect > video_aspect {
        // 窗口比视频更宽，以高度为基准
        let height = window_height;
        let width = (height as f32 * video_aspect) as u32;
        (width, height)
    } else {
        // 窗口比视频更窄，以宽度为基准
        let width = window_width;
        let height = (width as f32 / video_aspect) as u32;
        (width, height)
    };
    
    // 居中显示
    let x = ((window_width - w) / 2) as i32;
    let y = ((window_height - h) / 2) as i32;
    
    (x, y, w, h)
}
