//! Звук: сигнал при упоминании и проигрывание голосовых.
//!
//! Звуковая карта открывается один раз при запуске и держится всю сессию:
//! открывать её на каждый сигнал — это заметная задержка и щелчок в динамиках.
//!
//! Что можно проиграть, определяется набором декодеров rodio: wav, mp3, flac,
//! vorbis и aac в mp4. **Opus в этот список не входит**, а браузеры пишут
//! голосовые именно в него. Такие файлы открываются внешней программой —
//! см. `/open`.

use std::io::Cursor;

use rodio::{DeviceSinkBuilder, MixerDeviceSink, Player, Source, source::SineWave};

/// Длительность и громкость сигнала. Уведомление должно быть заметным,
/// но не пугать: в наушниках это играет прямо в ухо.
const CHIME_MS: u64 = 90;
const CHIME_GAIN: f32 = 0.12;

pub struct Sound {
    device: Option<MixerDeviceSink>,
    /// Проигрываемое голосовое. Хранится, потому что звук идёт, пока живёт
    /// этот объект: уронив его сразу, мы услышали бы только щелчок.
    voice: Option<Player>,
}

impl Sound {
    /// Открывает устройство. Отсутствие звуковой карты — не повод падать:
    /// чат должен работать и на машине вообще без звука.
    pub fn open() -> Self {
        let device = DeviceSinkBuilder::open_default_sink().ok().map(|mut sink| {
            // Иначе rodio при выходе пишет диагностику прямо поверх интерфейса.
            sink.log_on_drop(false);
            sink
        });
        Self {
            device,
            voice: None,
        }
    }

    pub fn silent() -> Self {
        Self {
            device: None,
            voice: None,
        }
    }

    pub fn is_available(&self) -> bool {
        self.device.is_some()
    }

    /// Короткий двухтоновый сигнал.
    ///
    /// Генерируется на месте, а не берётся из файла: звуковой файл пришлось бы
    /// куда-то класть и как-то ставить вместе с программой.
    pub fn chime(&self) {
        let Some(device) = &self.device else {
            return;
        };

        let first = SineWave::new(880.0)
            .take_duration(std::time::Duration::from_millis(CHIME_MS))
            .amplify(CHIME_GAIN);
        let second = SineWave::new(1320.0)
            .take_duration(std::time::Duration::from_millis(CHIME_MS))
            .amplify(CHIME_GAIN)
            // Плавное затухание: обрыв синуса на полуволне слышен как щелчок.
            .fade_out(std::time::Duration::from_millis(CHIME_MS));

        let player = Player::connect_new(device.mixer());
        player.append(first);
        player.append(second);
        // Отпускаем: звук доиграет сам, а держать его нам незачем.
        player.detach();
    }

    /// Проигрывает голосовое из скачанных байтов.
    ///
    /// `Err` с внятной причиной, если формат не поддержан: чаще всего это
    /// opus из браузера.
    pub fn play_voice(&mut self, bytes: Vec<u8>) -> Result<(), String> {
        if self.device.is_none() {
            return Err("звуковое устройство недоступно".into());
        }
        // Предыдущее голосовое останавливаем: два сразу — это каша.
        self.stop_voice();

        let Some(device) = &self.device else {
            return Err("звуковое устройство недоступно".into());
        };
        let player = rodio::play(device.mixer(), Cursor::new(bytes)).map_err(|err| {
            format!("не удалось проиграть: {err}. Возможно, это opus — тогда поможет /open")
        })?;
        self.voice = Some(player);
        Ok(())
    }

    pub fn stop_voice(&mut self) {
        if let Some(player) = self.voice.take() {
            player.stop();
        }
    }

    /// Играет ли что-то прямо сейчас.
    pub fn is_playing(&self) -> bool {
        self.voice
            .as_ref()
            .is_some_and(|player| !player.is_paused() && !player.empty())
    }
}

impl Default for Sound {
    fn default() -> Self {
        Self::silent()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_is_safe_to_use() {
        let mut sound = Sound::silent();

        // Ни один из вызовов не должен падать на машине без звука.
        sound.chime();
        sound.stop_voice();
        assert!(!sound.is_available());
        assert!(!sound.is_playing());
        assert!(sound.play_voice(vec![1, 2, 3]).is_err());
    }

    #[test]
    fn unsupported_format_explains_itself() {
        let mut sound = Sound::silent();

        let error = sound.play_voice(vec![0; 16]).unwrap_err();

        assert!(!error.is_empty());
    }
}
