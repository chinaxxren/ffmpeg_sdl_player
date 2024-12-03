extern crate ffmpeg_next as ffmpeg;

pub mod player;
pub mod video;
pub mod audio;

pub use player::{Player, ControlCommand};