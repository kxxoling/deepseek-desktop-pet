//! 音频输出基座：全局持有一个输出流，按字节数据播放音效。
//! 选型 rodio（纯 Rust、基于 `cpal`）：API 规模适中，`cpal` 过底层、`kira` 偏游戏引擎。
//! 无音频设备的机器（CI）上初始化失败只记日志，播放调用全部变空操作。

use std::io::Cursor;
use std::sync::OnceLock;

/// 全局输出流句柄；`None` 表示本机没有可用音频输出
static HANDLE: OnceLock<Option<rodio::OutputStreamHandle>> = OnceLock::new();

/// 启动时初始化输出流；没有设备时返回 false（后续播放为空操作）
pub fn init() -> bool {
    let got = rodio::OutputStream::try_default()
        .map(|(stream, handle)| {
            // 流对象必须保活，句柄才有效；泄漏一个流对象是有意的
            std::mem::forget(stream);
            handle
        })
        .map_err(|e| log::info!("无可用音频输出：{e}"))
        .ok();
    let ok = got.is_some();
    HANDLE.set(got).is_ok() && ok
}

/// 播放一段内存中的音频字节（按容器格式解码，目前启用 wav）。
/// 有输出流时异步播放，函数立刻返回；解码失败记日志
#[allow(dead_code)]
pub fn play(data: &[u8]) {
    let Some(handle) = HANDLE.get().and_then(Option::as_ref) else {
        return;
    };
    match rodio::Decoder::new(Cursor::new(data.to_vec())) {
        Ok(source) => {
            if let Err(e) = rodio::Sink::try_new(handle).map(|sink| {
                sink.append(source);
                sink.detach();
            }) {
                log::warn!("音频播放失败：{e}");
            }
        }
        Err(e) => log::warn!("音频解码失败：{e}"),
    }
}

/// 是否有可用的音频输出（初始化成功过）
#[allow(dead_code)]
pub fn available() -> bool {
    HANDLE.get().is_some_and(std::option::Option::is_some)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 生成一个极小的合法 WAV（16-bit 单声道 44100Hz，几个采样点的静音）
    fn tiny_wav() -> Vec<u8> {
        const RATE: u32 = 44_100;
        const SAMPLES: usize = 8;
        let data_len = SAMPLES * 2;
        let mut buf = Vec::new();
        buf.extend_from_slice(b"RIFF");
        buf.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
        buf.extend_from_slice(b"WAVE");
        buf.extend_from_slice(b"fmt ");
        buf.extend_from_slice(&16u32.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
        buf.extend_from_slice(&1u16.to_le_bytes()); // 单声道
        buf.extend_from_slice(&RATE.to_le_bytes());
        buf.extend_from_slice(&(RATE * 2).to_le_bytes()); // 字节率
        buf.extend_from_slice(&2u16.to_le_bytes()); // 块对齐
        buf.extend_from_slice(&16u16.to_le_bytes()); // 位深
        buf.extend_from_slice(b"data");
        buf.extend_from_slice(&(data_len as u32).to_le_bytes());
        buf.resize(buf.len() + data_len, 0);
        buf
    }

    #[test]
    fn decodes_wav_bytes() {
        // 只验证解码路径：CI 无音频设备，init 前的 play 是空操作但不报错
        assert!(!available());
        play(&tiny_wav()); // 无设备时的空操作
        // 解码器能读出这个 wav（不依赖输出设备）
        let decoded = rodio::Decoder::new(Cursor::new(tiny_wav()));
        assert!(decoded.is_ok());
        let mut samples = decoded.unwrap();
        assert!(samples.next().is_some());
    }

    #[test]
    fn garbage_bytes_do_not_panic() {
        play(b"not audio at all");
    }
}
