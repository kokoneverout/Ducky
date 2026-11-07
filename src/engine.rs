use crate::{
    api,
    audio::{self, Recording},
    config::Config,
    platform::{self, Target},
};
use anyhow::Result;
use std::{
    sync::{
        Arc, RwLock,
        atomic::Ordering,
        mpsc::{self, RecvTimeoutError},
    },
    time::{Duration, Instant},
};

pub enum Command {
    Press { at: Instant, target: Target },
    Release { at: Instant },
}

pub enum Event {
    Recording,
    Recognizing,
    Polishing,
    Notice(String),
    Error(String),
    AnimationError(String),
    Finished {
        raw: String,
        text: String,
        model: String,
        status: String,
    },
    Preview(crate::config::AnimationConfig),
    Quit,
}

pub fn long_enough(held: Duration, samples: usize) -> bool {
    held >= Duration::from_secs(1) && samples >= audio::SAMPLE_RATE as usize
}

fn polish_or_original(raw: String, result: Result<String>) -> (String, String) {
    match result {
        Ok(text) if !text.trim().is_empty() => (text, String::new()),
        Ok(_) => (raw, "润色返回空内容，已使用原文。".into()),
        Err(error) => (raw, format!("润色失败，已使用原文：{error}。 ")),
    }
}

pub fn start(config: Arc<RwLock<Config>>, events: async_channel::Sender<Event>) -> Result<()> {
    let (commands, receive) = mpsc::channel();
    let client = api::client()?;
    platform::start_hook(commands)?;
    std::thread::Builder::new()
        .name("dictation".into())
        .spawn(move || {
            let emit = |event| {
                let _ = events.send_blocking(event);
            };
            let mut active: Option<(Recording, Instant, Target, Config)> = None;
            loop {
                let command = if let Some((_, at, _, _)) = &active {
                    receive.recv_timeout(
                        Duration::from_secs(audio::MAX_SECONDS).saturating_sub(at.elapsed()),
                    )
                } else {
                    receive.recv().map_err(|_| RecvTimeoutError::Disconnected)
                };
                match command {
                    Ok(Command::Press { at, target }) => {
                        let config = config.read().unwrap_or_else(|e| e.into_inner()).clone();
                        match config.validate().and_then(|_| Recording::start()) {
                            Ok(recording) => {
                                active = Some((recording, at, target, config));
                                emit(Event::Recording);
                            }
                            Err(error) => {
                                emit(Event::Error(format!("{error:#}")));
                                // Keep the gesture reserved until its matching key-up.
                                // Clearing here could let an older Release unlock a newer recording.
                            }
                        }
                    }
                    Ok(Command::Release { at: released }) => {
                        if let Some((recording, pressed, target, config)) = active.take() {
                            // Check hold time BEFORE any network call, even if the mic generated no samples.
                            if released.saturating_duration_since(pressed) < Duration::from_secs(1)
                            {
                                drop(recording);
                                emit(Event::Notice("录音短于 1 秒，已丢弃".into()));
                            } else {
                                let result = recording.finish().and_then(|samples| {
                                    if !long_enough(
                                        released.saturating_duration_since(pressed),
                                        samples.len(),
                                    ) {
                                        emit(Event::Notice("有效音频不足 1 秒，已丢弃".into()));
                                        return Ok(());
                                    }
                                    emit(Event::Recognizing);
                                    let raw = api::transcribe(&client, &config, &samples)?;
                                    drop(samples);
                                    let original = raw.clone();
                                    let (text, warning) = if config.polish_enabled {
                                        emit(Event::Polishing);
                                        let result = api::polish(&client, &config, &raw);
                                        polish_or_original(raw, result)
                                    } else {
                                        (raw, String::new())
                                    };
                                    // Keep the final text even when clipboard/input APIs fail.
                                    let status = match platform::insert(&text, target) {
                                        Ok(status) => format!("{warning}{status}"),
                                        Err(error) => format!(
                                            "{warning}上屏失败：{error}；请点击复制结果重试"
                                        ),
                                    };
                                    emit(Event::Finished {
                                        raw: original,
                                        text,
                                        model: config.recognition_model().to_string(),
                                        status,
                                    });
                                    Ok(())
                                });
                                if let Err(error) = result {
                                    emit(Event::Error(format!("{error:#}")));
                                }
                            }
                        }
                        platform::BUSY.store(false, Ordering::SeqCst);
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        active = None;
                        emit(Event::Error(
                            "单次录音最多 3 分钟，本次已丢弃。松开右 Ctrl 后重新录音".into(),
                        ));
                        // Leave BUSY set until the corresponding key-up is received.
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_second_threshold_includes_exact_boundary() {
        assert!(!long_enough(Duration::from_millis(999), 100_000));
        assert!(!long_enough(Duration::from_secs(2), 15_999));
        assert!(long_enough(Duration::from_secs(1), 16_000));
    }

    #[test]
    fn failed_or_empty_polish_preserves_the_original() {
        for result in [Ok("  ".into()), Err(anyhow::anyhow!("HTTP 401"))] {
            let (text, warning) = polish_or_original("原始文稿".into(), result);
            assert_eq!(text, "原始文稿");
            assert!(!warning.is_empty());
        }
        let (text, warning) = polish_or_original("原文".into(), Ok("润色文稿".into()));
        assert_eq!(text, "润色文稿");
        assert!(warning.is_empty());
    }
}
