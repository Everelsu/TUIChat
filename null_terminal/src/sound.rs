//! Звук: сигнал при упоминании и проигрывание голосовых.
//!
//! Звуковая карта открывается один раз при запуске и держится всю сессию:
//! открывать её на каждый сигнал — это заметная задержка и щелчок в динамиках.
//!
//! Что можно проиграть, определяется набором декодеров rodio: wav, mp3, flac,
//! vorbis и aac в mp4. **Opus в этот список не входит**, а браузеры пишут
//! голосовые именно в него. Такие файлы открываются внешней программой —
//! см. `/open`.

use std::{
    io::Cursor,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use rodio::{
    ChannelCount, DeviceSinkBuilder, MixerDeviceSink, Player, SampleRate, Source,
    buffer::SamplesBuffer,
    cpal::traits::{DeviceTrait, HostTrait},
    microphone::MicrophoneBuilder,
    source::SineWave,
    source::UniformSourceIterator,
};

/// Голосовое пишем в моно 16 кГц: речи этого хватает с запасом, а пять
/// мегабайт потолка на файл превращаются из тридцати секунд в две с половиной
/// минуты.
const VOICE_RATE: u32 = 16_000;

/// Потолок на длину записи — та же защита, что и в браузере: забытая кнопка
/// не должна упереться в лимит размера уже после разговора.
const MAX_RECORD_SECONDS: u64 = 150;

/// Длительность сигнала. Уведомление должно быть заметным, но не пугать:
/// в наушниках это играет прямо в ухо.
const CHIME_MS: u64 = 90;

/// Три ступени громкости сигнала. Не ползунок: разница между тихим и громким
/// в наушниках и так велика, а крутить проценты в чате никто не станет.
pub const GAINS: [f32; 3] = [0.05, 0.12, 0.25];

/// Имя устройства, выбранного человеком, или «как в системе».
///
/// Храним именно имя, а не номер в списке: наушники втыкают и вынимают, и
/// после перезапуска третье устройство — уже не то же самое.
pub type Choice = Option<String>;

/// Как зовут устройства вывода. Пустой список означает, что звука на машине
/// нет вовсе — это не поломка, чат работает и молча.
pub fn outputs() -> Vec<String> {
    // Через cpal напрямую: перечисление динамиков в самом rodio пока спрятано
    // за экспериментальной фичей, а список устройств — не то место, ради
    // которого стоит на неё соглашаться.
    let Ok(devices) = rodio::cpal::default_host().output_devices() else {
        return Vec::new();
    };
    devices
        .filter_map(|device| Some(device.description().ok()?.name().to_string()))
        .collect()
}

/// Как зовут микрофоны.
pub fn inputs() -> Vec<String> {
    rodio::microphone::available_inputs()
        .map(|list| list.iter().map(|device| device.to_string()).collect())
        .unwrap_or_default()
}

/// Открывает вывод: названное устройство, а если его не нашлось — системное.
///
/// Не нашлось — обычное дело: наушники выдернули между запусками. Молча
/// вернуться к системному лучше, чем остаться без звука вовсе.
fn open_output(name: Option<&str>) -> Option<MixerDeviceSink> {
    let chosen = name.and_then(|name| {
        rodio::cpal::default_host()
            .output_devices()
            .ok()?
            .find(|device| device.description().is_ok_and(|found| found.name() == name))
    });

    let opened = chosen.and_then(|device| {
        DeviceSinkBuilder::from_device(device)
            .ok()?
            .open_stream()
            .ok()
    });
    let mut sink = match opened {
        Some(sink) => sink,
        None => DeviceSinkBuilder::open_default_sink().ok()?,
    };
    // Иначе rodio при выходе пишет диагностику прямо поверх интерфейса.
    sink.log_on_drop(false);
    Some(sink)
}

/// Идущая запись с микрофона.
struct Recording {
    stop: Arc<AtomicBool>,
    #[allow(clippy::type_complexity)]
    worker: std::thread::JoinHandle<Result<(Vec<f32>, ChannelCount, SampleRate), String>>,
    started: Instant,
}

pub struct Sound {
    device: Option<MixerDeviceSink>,
    recording: Option<Recording>,
    /// Проигрываемое голосовое. Хранится, потому что звук идёт, пока живёт
    /// этот объект: уронив его сразу, мы услышали бы только щелчок.
    voice: Option<Player>,
    /// Микрофон, выбранный человеком. Открывается на время записи, а не
    /// заранее: держать открытым микрофон, которым не пользуются, — верный
    /// способ засветить индикатор записи в системе на всю сессию.
    input: Choice,
    /// Громкость сигнала — номер ступени в [`GAINS`].
    gain: usize,
}

impl Sound {
    /// Открывает устройство. Отсутствие звуковой карты — не повод падать:
    /// чат должен работать и на машине вообще без звука.
    pub fn open() -> Self {
        Self {
            device: open_output(None),
            recording: None,
            voice: None,
            input: None,
            gain: 1,
        }
    }

    pub fn silent() -> Self {
        Self {
            device: None,
            recording: None,
            voice: None,
            input: None,
            gain: 1,
        }
    }

    /// Переключает устройства на ходу.
    ///
    /// Вывод переоткрывается сразу — иначе непонятно, подействовал ли выбор;
    /// микрофон только запоминается, он открывается на время записи.
    /// `false` означает, что вывод открыть не удалось: звука не будет, и
    /// сказать об этом надо.
    pub fn use_devices(&mut self, output: Option<&str>, input: Choice) -> bool {
        self.stop_voice();
        self.device = open_output(output);
        self.input = input;
        self.device.is_some()
    }

    /// Ступень громкости сигнала.
    pub fn set_gain(&mut self, step: usize) {
        self.gain = step.min(GAINS.len() - 1);
    }

    pub fn is_recording(&self) -> bool {
        self.recording.is_some()
    }

    /// Сколько секунд уже пишем.
    pub fn recorded_seconds(&self) -> u64 {
        self.recording
            .as_ref()
            .map_or(0, |recording| recording.started.elapsed().as_secs())
    }

    /// Начинает запись с микрофона.
    ///
    /// Микрофон открывается в отдельном потоке и там же вычитывается: его
    /// итератор блокируется до появления данных, и держать этим цикл
    /// отрисовки нельзя.
    pub fn start_recording(&mut self) -> Result<(), String> {
        if self.recording.is_some() {
            return Err("запись уже идёт".into());
        }

        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let chosen = self.input.clone();
        let worker = std::thread::spawn(move || {
            let builder = MicrophoneBuilder::new();
            // Выбранный микрофон, а если его больше нет — системный: гарнитуру
            // выдёргивают, и запись из-за этого падать не должна.
            let named = chosen.as_deref().and_then(|name| {
                rodio::microphone::available_inputs()
                    .ok()?
                    .into_iter()
                    .find(|device| device.to_string() == name)
            });
            let device = match named.and_then(|device| builder.device(device).ok()) {
                Some(device) => device,
                None => builder
                    .default_device()
                    .map_err(|err| format!("микрофон не найден: {err}"))?,
            };
            let mic = device
                .default_config()
                .map_err(|err| format!("микрофон не настроить: {err}"))?
                .open_stream()
                .map_err(|err| format!("микрофон не открыть: {err}"))?;

            let channels = mic.channels();
            let rate = mic.sample_rate();
            let limit = MAX_RECORD_SECONDS as usize * rate.get() as usize * channels.get() as usize;

            let mut samples = Vec::new();
            for sample in mic {
                samples.push(sample);
                if flag.load(Ordering::Relaxed) || samples.len() >= limit {
                    break;
                }
            }
            Ok((samples, channels, rate))
        });

        self.recording = Some(Recording {
            stop,
            worker,
            started: Instant::now(),
        });
        Ok(())
    }

    /// Останавливает запись и отдаёт готовый wav.
    ///
    /// Именно wav: браузеры играют его нативно, а opus нам не закодировать без
    /// C-библиотеки. Для голосового сообщения это ровно то, что нужно.
    pub fn stop_recording(&mut self) -> Result<Vec<u8>, String> {
        let Some(recording) = self.recording.take() else {
            return Err("запись не шла".into());
        };
        recording.stop.store(true, Ordering::Relaxed);

        let (samples, channels, rate) = recording
            .worker
            .join()
            .map_err(|_| "поток записи сорвался".to_string())??;
        if samples.is_empty() {
            return Err("ничего не записалось".into());
        }

        // Приводим к моно 16 кГц: разговор от этого не страдает, а размер
        // падает в разы.
        let source = SamplesBuffer::new(channels, rate, samples);
        let target = SampleRate::new(VOICE_RATE).ok_or("неверная частота")?;
        let mono = UniformSourceIterator::new(source, ChannelCount::new(1).unwrap(), target);
        let pcm: Vec<i16> = mono
            .map(|sample| (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
            .collect();
        Ok(wav(&pcm, VOICE_RATE))
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

        let gain = GAINS[self.gain.min(GAINS.len() - 1)];
        let first = SineWave::new(880.0)
            .take_duration(std::time::Duration::from_millis(CHIME_MS))
            .amplify(gain);
        let second = SineWave::new(1320.0)
            .take_duration(std::time::Duration::from_millis(CHIME_MS))
            .amplify(gain)
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

/// Собирает моно wav из 16-битных отсчётов.
///
/// Заголовок пишем руками: он занимает сорок четыре байта по спецификации,
/// и тащить ради него ещё одну зависимость незачем.
fn wav(samples: &[i16], rate: u32) -> Vec<u8> {
    const HEADER: usize = 44;
    let data = samples.len() * 2;
    let mut out = Vec::with_capacity(HEADER + data);

    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((HEADER - 8 + data) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");

    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // длина блока
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM без сжатия
    out.extend_from_slice(&1u16.to_le_bytes()); // моно
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes()); // байт в секунду
    out.extend_from_slice(&2u16.to_le_bytes()); // байт на кадр
    out.extend_from_slice(&16u16.to_le_bytes()); // бит на отсчёт

    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data as u32).to_le_bytes());
    for sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
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
    fn wav_header_describes_the_data() {
        let bytes = wav(&[0, 1, -1, 32767], 16_000);

        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[36..40], b"data");
        // Заголовок плюс два байта на отсчёт.
        assert_eq!(bytes.len(), 44 + 8);
        assert_eq!(
            u32::from_le_bytes(bytes[40..44].try_into().unwrap()),
            8,
            "в заголовке неверная длина данных"
        );
        // Частоту тоже должно быть видно: без неё голос играется не тем темпом.
        assert_eq!(
            u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            16_000
        );
    }

    #[test]
    fn recorded_wav_is_recognized_by_the_server() {
        let bytes = wav(&[0; 16], 16_000);

        // Сервер узнаёт формат по сигнатуре: записанное из терминала должно
        // проходить его проверку, иначе отправить это будет нельзя.
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
    }

    #[test]
    fn stopping_without_recording_is_reported() {
        let mut sound = Sound::silent();

        assert!(!sound.is_recording());
        assert_eq!(sound.recorded_seconds(), 0);
        assert!(sound.stop_recording().is_err());
    }

    #[test]
    fn unsupported_format_explains_itself() {
        let mut sound = Sound::silent();

        let error = sound.play_voice(vec![0; 16]).unwrap_err();

        assert!(!error.is_empty());
    }
}
