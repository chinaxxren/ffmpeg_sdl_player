extern crate ffmpeg_next as ffmpeg;

use ffmpeg::format::Pixel;
use ffmpeg::frame::Video;
use sdl2::pixels::PixelFormatEnum;
use sdl2::video::Window;
use std::error::Error;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use std::path::PathBuf;

mod audio;
mod player;
mod video;

use crate::player::Player;

// 默认窗口尺寸
static SC_WIDTH: AtomicU32 = AtomicU32::new(800);
static SC_HEIGHT: AtomicU32 = AtomicU32::new(600);

// 视频播放器配置
struct PlayerConfig {
    video_path: PathBuf,
    initial_width: u32,
    initial_height: u32,
}

// 窗口状态结构体
struct WindowState {
    size: (u32, u32),
    display_rect: Option<(i32, i32, u32, u32)>,
}

impl WindowState {
    fn new(width: u32, height: u32) -> Self {
        Self {
            size: (width, height),
            display_rect: None,
        }
    }

    // 处理窗口大小调整
    fn handle_resize(&mut self, new_width: u32, new_height: u32) {
        let new_size = (new_width, new_height);
        println!("处理窗口大小调整 - 当前: {}x{}, 新的: {}x{}", 
            self.size.0, self.size.1, new_width, new_height);
            
        if new_size != self.size {
            println!("窗口大小已更改为: {}x{}", new_width, new_height);
            self.size = new_size;
            self.display_rect = None;
            SC_WIDTH.store(new_width, Ordering::Relaxed);
            SC_HEIGHT.store(new_height, Ordering::Relaxed);
            println!("全局窗口大小已更新: {}x{}", 
                SC_WIDTH.load(Ordering::Relaxed),
                SC_HEIGHT.load(Ordering::Relaxed));
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

// SDL 相关状态
struct SdlContext {
    canvas: sdl2::render::Canvas<Window>,
    event_pump: sdl2::EventPump,
    texture_creator: sdl2::render::TextureCreator<sdl2::video::WindowContext>,
}

impl SdlContext {
    fn new(config: &PlayerConfig) -> Result<Self, Box<dyn Error>> {
        let sdl_context = sdl2::init()?;
        let video_subsystem = sdl_context.video()?;
        let window = video_subsystem
            .window("FFmpeg SDL Player", config.initial_width, config.initial_height)
            .position_centered()
            .resizable()
            .build()?;

        let canvas = window.into_canvas().build()?;
        let event_pump = sdl_context.event_pump()?;
        let texture_creator = canvas.texture_creator();

        Ok(Self {
            canvas,
            event_pump,
            texture_creator,
        })
    }
}

// FPS 计数器
struct FpsCounter {
    frame_count: u32,
    last_update: Instant,
}

impl FpsCounter {
    fn new() -> Self {
        Self {
            frame_count: 0,
            last_update: Instant::now(),
        }
    }

    fn update(&mut self) {
        self.frame_count += 1;
        if self.last_update.elapsed() >= Duration::from_secs(1) {
            println!("FPS: {}", self.frame_count);
            self.frame_count = 0;
            self.last_update = Instant::now();
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    // 初始化配置
    let config = PlayerConfig {
        video_path: "/Users/chinaxxren/Desktop/a.mp4".into(),
        initial_width: SC_WIDTH.load(Ordering::Relaxed),
        initial_height: SC_HEIGHT.load(Ordering::Relaxed),
    };

    println!("初始窗口大小设置为: {}x{}", config.initial_width, config.initial_height);

    println!("开始播放视频: {}", config.video_path.as_os_str().to_str().unwrap());

    // 初始化 SDL
    let mut sdl = SdlContext::new(&config)?;
    let (window_width, window_height) = sdl.canvas.output_size()?;
    println!("SDL窗口实际大小: {}x{}", window_width, window_height);
    
    let mut window_state = WindowState::new(window_width, window_height);
    let mut fps_counter = FpsCounter::new();
    let mut current_texture = None;
    let mut last_frame_time = Instant::now();

    // 创建视频帧通道
    let (frame_sender, frame_receiver) = mpsc::channel::<Video>();

    // 初始化播放器
    let player = Arc::new(Mutex::new(Player::start(
        config.video_path,
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

    // 主循环
    'running: loop {
        // 处理事件
        if !handle_events(&mut sdl.event_pump, &mut window_state, &player)? {
            break 'running;
        }

        // 处理视频帧
        match frame_receiver.try_recv() {
            Ok(frame) => {
                last_frame_time = Instant::now();
                process_video_frame(
                    frame,
                    &mut current_texture,
                    &sdl.texture_creator,
                    &mut sdl.canvas,
                    &mut window_state,
                )?;
                fps_counter.update();
            }
            Err(mpsc::TryRecvError::Empty) => {
                check_frame_timeout(last_frame_time, &player)?;
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                println!("播放器断开连接，退出循环");
                break 'running;
            }
        }
    }

    println!("主循环结束，开始清理资源");
    drop(player);
    println!("资源清理完成");

    Ok(())
}

// 处理事件
fn handle_events(
    event_pump: &mut sdl2::EventPump,
    window_state: &mut WindowState,
    player: &Arc<Mutex<Player>>,
) -> Result<bool, Box<dyn Error>> {
    for event in event_pump.poll_iter() {
        match event {
            sdl2::event::Event::Window { win_event, .. } => match win_event {
                sdl2::event::WindowEvent::Resized(x, y) |
                sdl2::event::WindowEvent::SizeChanged(x, y) => {
                    window_state.handle_resize(x as u32, y as u32);
                }
                _ => {}
            },
            sdl2::event::Event::Quit { .. } => {
                println!("接收到退出事件");
                return Ok(false);
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
    Ok(true)
}

// 缩放视频帧
fn rescaler_for_frame(frame: &Video) -> Video {
    // 创建新的视频帧，保持原始尺寸和格式
    let mut new_frame = Video::empty();
    let mut context = ffmpeg_next::software::scaling::Context::get(
        frame.format(),
        frame.width(),
        frame.height(),
        Pixel::YUV420P,
        frame.width(),  // 使用原始宽度
        frame.height(), // 使用原始高度
        ffmpeg::software::scaling::Flags::BILINEAR,
    )
    .unwrap();

    context.run(&frame, &mut new_frame).unwrap();
    new_frame
}

// 处理视频帧
fn process_video_frame<'a>(
    frame: Video,
    texture: &mut Option<sdl2::render::Texture<'a>>,
    texture_creator: &'a sdl2::render::TextureCreator<sdl2::video::WindowContext>,
    canvas: &mut sdl2::render::Canvas<Window>,
    window_state: &mut WindowState,
) -> Result<(), Box<dyn Error>> {
    let video_width = frame.width();
    let video_height = frame.height();

    // 创建或重新创建纹理（如果尺寸不匹配）
    if texture.is_none() {
        *texture = Some(texture_creator.create_texture_streaming(
            PixelFormatEnum::IYUV,
            video_width,
            video_height,
        )?);
    }

    if let Some(ref mut tex) = texture {
        // 更新纹理数据
        tex.update_yuv(
            None,
            frame.data(0),
            frame.stride(0),
            frame.data(1),
            frame.stride(1),
            frame.data(2),
            frame.stride(2),
        )?;

        // 获取窗口尺寸并更新显示区域
        let (window_width, window_height) = canvas.output_size()?;
        window_state.update_display_rect(
            window_width,
            window_height,
            video_width,
            video_height
        );
        
        let (x, y, w, h) = window_state.display_rect.unwrap();
        
        // 只清除一次画布
        canvas.set_draw_color(sdl2::pixels::Color::BLACK);
        canvas.clear();
        
        // 使用整数坐标以避免子像素渲染
        let src_rect = sdl2::rect::Rect::new(0, 0, video_width, video_height);
        let dst_rect = sdl2::rect::Rect::new(x, y, w, h);
        
        canvas.copy(tex, Some(src_rect), Some(dst_rect))?;
        canvas.present();
    }

    Ok(())
}

// 检查帧超时
fn check_frame_timeout(last_frame_time: Instant, player: &Arc<Mutex<Player>>) -> Result<(), Box<dyn Error>> {
    if last_frame_time.elapsed() > Duration::from_secs(5) {
        println!("警告: 5秒未收到新帧");
        if let Ok(_) = player.lock() {
            println!("Player 仍然存在且可访问");
        }
    }
    Ok(())
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
        let height = window_height;
        let width = (height as f32 * video_aspect) as u32;
        (width, height)
    } else {
        let width = window_width;
        let height = (width as f32 / video_aspect) as u32;
        (width, height)
    };
    
    let x = ((window_width - w) / 2) as i32;
    let y = ((window_height - h) / 2) as i32;
    
    (x, y, w, h)
}
