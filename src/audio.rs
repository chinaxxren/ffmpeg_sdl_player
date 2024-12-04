extern crate ffmpeg_next as ffmpeg;

use std::pin::Pin;

use bytemuck::Pod;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SizedSample;

use futures::future::OptionFuture;
use futures::FutureExt;
use ringbuf::ring_buffer::{RbRef, RbWrite};
use ringbuf::HeapRb;
use std::future::Future;

use crate::player::ControlCommand;

pub struct AudioPlaybackThread {
    control_sender: smol::channel::Sender<ControlCommand>,
    packet_sender: smol::channel::Sender<ffmpeg::codec::packet::packet::Packet>,
    receiver_thread: Option<std::thread::JoinHandle<()>>,
}

impl AudioPlaybackThread {
    pub fn start(stream: &ffmpeg::format::stream::Stream) -> Result<Self, anyhow::Error> {
        println!("音频线程启动 - 流信息: {}", stream.duration());

        let (control_sender, control_receiver) = smol::channel::unbounded();

        let (packet_sender, packet_receiver) = smol::channel::bounded(128);

        let decoder_context = ffmpeg::codec::Context::from_parameters(stream.parameters())?;
        let packet_decoder = decoder_context.decoder().audio()?;

        println!("音频解码器初始化完成 - 格式: {:?}", packet_decoder.format());

        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .expect("no output device available");
        println!("音频输出设备: {:?}", device.name());

        let config = device.default_output_config().unwrap();
        println!(
            "音频输出配置 - 采样率: {}, 通道: {}, 格式: {:?}",
            config.sample_rate().0,
            config.channels(),
            config.sample_format()
        );

        let receiver_thread = std::thread::Builder::new()
            .name("audio playback thread".into())
            .spawn(move || {
                smol::block_on(async move {
                    let output_channel_layout = match config.channels() {
                        1 => ffmpeg::util::channel_layout::ChannelLayout::MONO,
                        2 => ffmpeg::util::channel_layout::ChannelLayout::STEREO,
                        _ => todo!(),
                    };
                    println!("音频输出通道布局: {:?}", output_channel_layout);

                    let mut ffmpeg_to_cpal_forwarder = match config.sample_format() {
                        cpal::SampleFormat::U8 => {
                            println!("使用U8采样格式");
                            FFmpegToCPalForwarder::new::<u8>(
                                config,
                                &device,
                                packet_receiver,
                                packet_decoder,
                                ffmpeg::util::format::sample::Sample::U8(
                                    ffmpeg::util::format::sample::Type::Packed,
                                ),
                                output_channel_layout,
                            )
                        }
                        cpal::SampleFormat::F32 => {
                            println!("使用F32采样格式");
                            FFmpegToCPalForwarder::new::<f32>(
                                config,
                                &device,
                                packet_receiver,
                                packet_decoder,
                                ffmpeg::util::format::sample::Sample::F32(
                                    ffmpeg::util::format::sample::Type::Packed,
                                ),
                                output_channel_layout,
                            )
                        }
                        format @ _ => todo!("unsupported cpal output format {:#?}", format),
                    };

                    let packet_receiver_impl = async { ffmpeg_to_cpal_forwarder.stream().await }
                        .fuse()
                        .shared();

                    let mut playing = true;

                    loop {
                        println!("waiting for packet");
                        let packet_receiver: OptionFuture<_> = if playing {
                            Some(packet_receiver_impl.clone())
                        } else {
                            None
                        }
                        .into();

                        println!("waiting for command");
                        smol::pin!(packet_receiver);

                        futures::select! {
                            _ = packet_receiver => {},
                            received_command = control_receiver.recv().fuse() => {
                                match received_command {
                                    Ok(ControlCommand::Pause) => {
                                        println!("音频播放暂停");
                                        playing = false;
                                    }
                                    Ok(ControlCommand::Play) => {
                                        println!("音频播放开始");
                                        playing = true;
                                    }
                                    Err(e) => {
                                        println!("音频控制通道关闭 {}",e);
                                        return;
                                    }
                                }
                            }
                        }
                    }
                })
            })?;

        Ok(Self {
            control_sender,
            packet_sender,
            receiver_thread: Some(receiver_thread),
        })
    }

    pub async fn receive_packet(&self, packet: ffmpeg::codec::packet::packet::Packet) -> bool {
        match self.packet_sender.send(packet).await {
            Ok(_) => {
                // println!("音频包发送成功");
                true
            }
            Err(e) => {
                println!("音频包发送失败: {}", e);
                false
            }
        }
    }

    pub async fn send_control_message(&self, message: ControlCommand) {
        println!("发送音频控制消息: {:?}", message);
        if let Err(e) = self.control_sender.send(message).await {
            println!("发送音频控制消息失败: {}", e);
        }
    }
}

impl Drop for AudioPlaybackThread {
    fn drop(&mut self) {
        println!("AudioPlaybackThread drop");
        self.control_sender.close();
        if let Some(receiver_join_handle) = self.receiver_thread.take() {
            receiver_join_handle.join().unwrap();
        }
    }
}

trait FFMpegToCPalSampleForwarder {
    fn forward(
        &mut self,
        audio_frame: ffmpeg::frame::Audio,
    ) -> Pin<Box<dyn Future<Output = ()> + '_>>;
}

impl<T: Pod, R: RbRef> FFMpegToCPalSampleForwarder for ringbuf::Producer<T, R>
where
    <R as RbRef>::Rb: RbWrite<T>,
{
    fn forward(
        &mut self,
        audio_frame: ffmpeg::frame::Audio,
    ) -> Pin<Box<dyn Future<Output = ()> + '_>> {
        println!(
            "转发音频帧 - 采样数: {}, 通道数: {}, 格式: {:?}",
            audio_frame.samples(),
            audio_frame.channels(),
            audio_frame.format()
        );
        Box::pin(async move {
            // Audio::plane() returns the wrong slice size, so correct it by hand. See also
            // for a fix https://github.com/zmwangx/rust-ffmpeg/pull/104.
            let expected_bytes =
                audio_frame.samples() * audio_frame.channels() as usize * core::mem::size_of::<T>();
            let cpal_sample_data: &[T] =
                bytemuck::cast_slice(&audio_frame.data(0)[..expected_bytes]);

            while self.free_len() < cpal_sample_data.len() {
                smol::Timer::after(std::time::Duration::from_millis(16)).await;
            }

            // Buffer the samples for playback
            self.push_slice(cpal_sample_data);
        })
    }
}

struct FFmpegToCPalForwarder {
    _cpal_stream: cpal::Stream,
    ffmpeg_to_cpal_pipe: Box<dyn FFMpegToCPalSampleForwarder>,
    packet_receiver: smol::channel::Receiver<ffmpeg::codec::packet::packet::Packet>,
    packet_decoder: ffmpeg::decoder::Audio,
    resampler: ffmpeg::software::resampling::Context,
}

impl FFmpegToCPalForwarder {
    fn new<T: Send + Pod + SizedSample + 'static>(
        config: cpal::SupportedStreamConfig,
        device: &cpal::Device,
        packet_receiver: smol::channel::Receiver<ffmpeg::codec::packet::packet::Packet>,
        packet_decoder: ffmpeg::decoder::Audio,
        output_format: ffmpeg::util::format::sample::Sample,
        output_channel_layout: ffmpeg::util::channel_layout::ChannelLayout,
    ) -> Self {
        let buffer = HeapRb::new(4096);
        let (sample_producer, mut sample_consumer) = buffer.split();

        let cpal_stream = device
            .build_output_stream(
                &config.config(),
                move |data, _| {
                    let filled = sample_consumer.pop_slice(data);
                    data[filled..].fill(T::EQUILIBRIUM);
                },
                move |err| {
                    eprintln!("error feeding audio stream to cpal: {}", err);
                },
                None,
            )
            .unwrap();

        cpal_stream.play().unwrap();

        let resampler = ffmpeg::software::resampling::Context::get(
            packet_decoder.format(),
            packet_decoder.channel_layout(),
            packet_decoder.rate(),
            output_format,
            output_channel_layout,
            config.sample_rate().0,
        )
        .unwrap();

        Self {
            _cpal_stream: cpal_stream,
            ffmpeg_to_cpal_pipe: Box::new(sample_producer),
            packet_receiver,
            packet_decoder,
            resampler,
        }
    }

    async fn stream(&mut self) {
        println!("音频播放线程启动");
        loop {
            let Ok(packet) = self.packet_receiver.recv().await else {
                break;
            };

            // println!("音频包接收到");
            self.packet_decoder.send_packet(&packet).unwrap();

            let mut decoded_frame = ffmpeg::util::frame::Audio::empty();
            while self
                .packet_decoder
                .receive_frame(&mut decoded_frame)
                .is_ok()
            {
                println!("音频解码完成");
                let mut resampled_frame = ffmpeg::util::frame::Audio::empty();
                println!("音频重采样");
                self.resampler
                    .run(&decoded_frame, &mut resampled_frame)
                    .unwrap();
                println!("音频重采样完成");
                self.ffmpeg_to_cpal_pipe.forward(resampled_frame).await;
                println!("音频重采样结果发送给CPAL");
            }
        }
    }
}
