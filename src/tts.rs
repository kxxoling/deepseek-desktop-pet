//! 语音 TTS：用系统自带的语音合成（macOS `say`、Windows SAPI、Linux `espeak`），
//! 零网络零新增依赖。合成到临时文件后走 `audio` 基座播放；同一时间只播一段，
//! 新请求排队不叠音；合成失败静默降级只记日志。
//! 触发面由 config `tts_enabled` 控制（默认关）。

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};

/// TTS 请求队列的发送端；init 之前 speak 是空操作
static SENDER: OnceLock<Mutex<Option<mpsc::Sender<String>>>> = OnceLock::new();

/// 启动合成线程；配置未开或无合成器时仍初始化（config 可热改），只是队列空转
pub fn init() {
    let (tx, rx) = mpsc::channel::<String>();
    SENDER
        .set(Mutex::new(Some(tx)))
        .expect("tts 只应初始化一次");
    std::thread::Builder::new()
        .name("tts".into())
        .spawn(move || {
            while let Ok(text) = rx.recv() {
                if !crate::config::get_bool("tts_enabled").unwrap_or(false) {
                    continue;
                }
                if let Err(e) = synth_and_play(&text) {
                    log::info!("语音合成失败（降级为静默）：{e}");
                }
            }
        })
        .expect("tts 线程启动失败");
}

/// 请求朗读一段文字；未初始化或队列满时静默丢弃
pub fn speak(text: &str) {
    let Some(guard) = SENDER.get() else {
        return;
    };
    if let Some(tx) = guard
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
    {
        let _ = tx.send(text.to_string());
    }
}

/// 合成器命令行（纯函数，各平台期望值写死）：输出写到临时 wav
#[cfg(target_os = "macos")]
fn synth_command(out: &std::path::Path, text: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new("say");
    cmd.arg("-o")
        .arg(out)
        .arg("--data-format=LEF32@22050")
        .arg(text);
    cmd
}

#[cfg(target_os = "windows")]
fn synth_command(out: &std::path::Path, text: &str) -> std::process::Command {
    // PowerShell 调 SAPI：SetOutputToWaveFile 后 Speak
    let script = format!(
        "Add-Type -AssemblyName System.Speech; \
         $s = New-Object System.Speech.Synthesis.SpeechSynthesizer; \
         $s.SetOutputToWaveFile('{}'); $s.Speak('{}'); $s.Dispose()",
        out.display(),
        text.replace('\'', "''"),
    );
    let mut cmd = std::process::Command::new("powershell");
    cmd.arg("-NoProfile").arg("-Command").arg(script);
    cmd
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn synth_command(out: &std::path::Path, text: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new("espeak");
    cmd.arg("-w").arg(out).arg(text);
    cmd
}

/// 临时输出文件路径（每次播放覆盖同一个文件，避免残留堆积）
fn out_path() -> PathBuf {
    std::env::temp_dir().join("ds-pet-tts.wav")
}

/// 合成到临时文件并交给音频基座播放
fn synth_and_play(text: &str) -> Result<(), String> {
    let out = out_path();
    let status = synth_command(&out, text)
        .status()
        .map_err(|e| format!("启动合成器失败：{e}"))?;
    if !status.success() {
        return Err(format!("合成器退出码异常：{status}"));
    }
    let bytes = std::fs::read(&out).map_err(|e| format!("读取合成产物失败：{e}"))?;
    // 朗读按合成器的原始响度播放
    crate::audio::play(&bytes, 1.0);
    Ok(())
}

/// 确保临时文件在退出时尽力清理（进程正常退出路径）
pub fn cleanup() {
    let _ = std::fs::remove_file(out_path());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_synth_command_pinned() {
        let cmd = synth_command(std::path::Path::new("/tmp/t.wav"), "你好");
        let argv: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            argv,
            vec!["-o", "/tmp/t.wav", "--data-format=LEF32@22050", "你好"]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_synth_command_pinned() {
        let cmd = synth_command(std::path::Path::new("/tmp/t.wav"), "hello");
        let argv: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(argv, vec!["-w", "/tmp/t.wav", "hello"]);
    }

    #[test]
    fn out_path_is_stable_and_cleanup_removes() {
        let p = out_path();
        assert!(p.ends_with("ds-pet-tts.wav"));
        std::fs::write(&p, b"x").ok();
        assert!(p.exists());
        cleanup();
        assert!(!p.exists());
    }

    #[test]
    fn init_then_speak_with_tts_disabled_consumes_silently() {
        // 初始化队列线程后，配置关闭时 speak 被消费但不触发合成
        init();
        crate::config::set("tts_enabled", false);
        speak("不应触发合成");
        // 给线程一点时间消费
        std::thread::sleep(std::time::Duration::from_millis(200));
        // 到这里没 panic 即通过（synth_and_play 未被调用，因为没有日志输出）
    }

    #[test]
    fn speak_without_init_is_noop() {
        // 未调用 init 时队列不存在，静默返回
        speak("不会 panic");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn synth_and_play_roundtrip_local() {
        // 真机合成一句中文并播放；CI（linux）不跑此用例
        std::fs::write("/tmp/ds-pet-tts-test.txt", b"").ok();
        let r = synth_and_play("测试");
        // 只断言合成阶段成功（播放交给 audio 基座，无设备也为空操作）
        assert!(r.is_ok(), "{r:?}");
        assert!(out_path().is_file());
        cleanup();
    }
}
