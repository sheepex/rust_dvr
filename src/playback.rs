use std::time::{Duration, Instant};

use jiff::{civil::DateTime, tz::TimeZone, SignedDuration, Timestamp, Zoned};
use url::Url;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum Speed {
    Quarter,
    Half,
    #[default]
    Normal,
    Double,
    Quadruple,
}

impl Speed {
    pub const ALL: [Self; 5] = [
        Self::Quarter,
        Self::Half,
        Self::Normal,
        Self::Double,
        Self::Quadruple,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Quarter => "0,25×",
            Self::Half => "0,5×",
            Self::Normal => "1×",
            Self::Double => "2×",
            Self::Quadruple => "4×",
        }
    }

    pub fn rate(self) -> f64 {
        match self {
            Self::Quarter => 0.25,
            Self::Half => 0.5,
            Self::Normal => 1.0,
            Self::Double => 2.0,
            Self::Quadruple => 4.0,
        }
    }
}

/// Format up to 14 digits as AAAA-MM-DD HH:MM:SS while retaining partial input.
pub fn mask_datetime(input: &str) -> String {
    let mut result = String::new();
    for (index, digit) in input
        .chars()
        .filter(|c| c.is_ascii_digit())
        .take(14)
        .enumerate()
    {
        match index {
            4 | 6 => result.push('-'),
            8 => result.push(' '),
            10 | 12 => result.push(':'),
            _ => {}
        }
        result.push(digit);
    }
    result
}

pub fn cursor_after_digits(input: &str, digits: usize) -> usize {
    if digits == 0 {
        return 0;
    }
    input
        .chars()
        .enumerate()
        .filter(|(_, c)| c.is_ascii_digit())
        .nth(digits - 1)
        .map_or(input.chars().count(), |(i, _)| i + 1)
}

const FORM: &str = "%Y-%m-%d %H:%M:%S";

pub struct OpeningTimes {
    local: Interval,
    utc: Interval,
    offset_minutes: Option<i32>,
}

impl OpeningTimes {
    pub fn new(now: Zoned) -> Self {
        let end = Timestamp::from_second(now.timestamp().as_second())
            .expect("segundo de um instante válido")
            .to_zoned(now.time_zone().clone());
        let day = end.date();
        let start = [day.at(7, 30, 0, 0), day.at(0, 0, 0, 0)]
            .into_iter()
            .filter(|time| *time < end.datetime())
            .filter_map(|time| time.to_zoned(end.time_zone().clone()).ok())
            .find(|time| time.timestamp() < end.timestamp() && time.datetime() < end.datetime())
            .unwrap_or_else(|| {
                // At exactly midnight there is no earlier second on the same date.
                let civil = end.datetime() - SignedDuration::from_secs(1);
                civil
                    .to_zoned(end.time_zone().clone())
                    .expect("data local imediatamente anterior")
            });
        let offset = end.offset().seconds();
        let offset_minutes =
            (start.offset() == end.offset() && offset % 60 == 0 && offset.abs() <= 14 * 3600)
                .then_some(offset / 60);
        Self {
            local: Interval {
                start: start.datetime(),
                end: end.datetime(),
            },
            utc: Interval {
                start: start.timestamp().to_zoned(TimeZone::UTC).datetime(),
                end: end.timestamp().to_zoned(TimeZone::UTC).datetime(),
            },
            offset_minutes,
        }
    }

    pub fn fields(&self, utc: bool) -> [String; 2] {
        let interval = if utc { self.utc } else { self.local };
        [interval.start, interval.end].map(|time| time.strftime(FORM).to_string())
    }

    pub fn offset_field(&self) -> Option<String> {
        let minutes = self.offset_minutes?;
        let sign = if minutes < 0 { '-' } else { '+' };
        let minutes = minutes.abs();
        Some(format!("{sign}{:02}:{:02}", minutes / 60, minutes % 60))
    }
}

fn parse_datetime(input: &str) -> Result<DateTime, &'static str> {
    let bytes = input.as_bytes();
    if bytes.len() != 19
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b' '
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes
            .iter()
            .enumerate()
            .any(|(i, b)| !matches!(i, 4 | 7 | 10 | 13 | 16) && !b.is_ascii_digit())
    {
        return Err("Use o formato AAAA-MM-DD HH:MM:SS.");
    }
    if &input[..4] == "0000" || &input[17..] > "59" {
        return Err("Data ou hora inválida.");
    }
    DateTime::strptime(FORM, input).map_err(|_| "Data ou hora inválida.")
}

#[derive(Clone, Copy)]
pub struct Interval {
    start: DateTime,
    end: DateTime,
}

impl Interval {
    pub fn new(start: &str, end: &str) -> Result<Self, &'static str> {
        let start = parse_datetime(start)?;
        let end = parse_datetime(end)?;
        if start >= end {
            return Err("O horário final deve ser posterior ao inicial.");
        }
        Ok(Self { start, end })
    }

    pub fn seconds(self) -> i64 {
        self.start.duration_until(self.end).as_secs()
    }

    pub fn seek_offset(self, seconds: i64) -> i64 {
        seconds.clamp(0, self.seconds() - 1)
    }

    pub fn at(self, offset: i64) -> DateTime {
        // Clamping keeps the civil time inside the already validated interval.
        self.start
            .checked_add(SignedDuration::from_secs(offset.clamp(0, self.seconds())))
            .expect("offset dentro do intervalo")
    }

    pub fn label_at(self, offset: i64) -> String {
        self.at(offset).strftime(FORM).to_string()
    }

    pub fn shifted(self, minutes: i64) -> Result<Self, &'static str> {
        let delta = SignedDuration::from_mins(minutes);
        let invalid = |_| "O ajuste de fuso deixou o horário fora do intervalo válido.";
        let start = self.start.checked_add(delta).map_err(invalid)?;
        let end = self.end.checked_add(delta).map_err(invalid)?;
        if start.year() < 1 {
            return Err("O ajuste de fuso deixou o horário fora do intervalo válido.");
        }
        Ok(Self { start, end })
    }

    pub fn url_at(
        self,
        base: Url,
        brand: crate::Brand,
        channel: u32,
        offset: i64,
    ) -> Result<Url, &'static str> {
        url_for_times(
            base,
            brand,
            channel,
            self.at(self.seek_offset(offset)),
            self.end,
        )
    }
}

pub fn utc_offset_minutes(input: &str) -> Result<i64, &'static str> {
    let bytes = input.trim().as_bytes();
    match bytes {
        [sign @ (b'+' | b'-'), h1, h2, b':', m1, m2]
            if [h1, h2, m1, m2].into_iter().all(u8::is_ascii_digit) =>
        {
            let number = |a, b| i64::from((a - b'0') * 10 + (b - b'0'));
            let (hour, minute) = (number(*h1, *h2), number(*m1, *m2));
            if minute < 60 && (hour < 14 || (hour == 14 && minute == 0)) {
                return Ok((hour * 60 + minute) * if *sign == b'-' { -1 } else { 1 });
            }
        }
        _ => {}
    }
    Err("Informe o fuso dos Intelbras em relação a UTC (ex.: -03:00).")
}

pub const FRAME_STALL: Duration = Duration::from_secs(4);
const MAX_FRAME_LAG: f64 = 10.0; // Seconds on the recording timeline.

pub fn startup_grace(prefill_seconds: f64) -> Duration {
    Duration::from_secs_f64((prefill_seconds + 5.0).max(6.0))
}

#[derive(Clone, Copy)]
pub struct SyncClock {
    offset: i64,
    speed: Speed,
    waiting_since: Instant,
    startup_grace: Duration,
    started: Option<Instant>,
}

impl SyncClock {
    pub fn new(offset: i64, speed: Speed, prefill_seconds: f64, now: Instant) -> Self {
        Self {
            offset,
            speed,
            waiting_since: now,
            startup_grace: startup_grace(prefill_seconds),
            started: None,
        }
    }

    pub fn tick(&mut self, now: Instant, ready: bool) {
        if self.started.is_none()
            && (ready || now.duration_since(self.waiting_since) >= self.startup_grace)
        {
            self.started = Some(now);
        }
    }

    pub fn started(self) -> bool {
        self.started.is_some()
    }

    pub fn speed(self) -> Speed {
        self.speed
    }

    pub fn position(self, now: Instant) -> f64 {
        self.offset as f64
            + self.started.map_or(0.0, |start| {
                now.duration_since(start).as_secs_f64() * self.speed.rate()
            })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum FrameTiming {
    Present,
    Wait,
    Drop,
}

pub fn due_through(position: f64, speed: Speed) -> f64 {
    position + 0.005 * speed.rate()
}

pub fn frame_timing(frame: f64, position: f64, speed: Speed) -> FrameTiming {
    if !frame.is_finite() || frame < position - MAX_FRAME_LAG {
        FrameTiming::Drop
    } else if frame > due_through(position, speed) {
        // Don't jump ahead into a future recording when a camera has a gap.
        FrameTiming::Wait
    } else {
        FrameTiming::Present
    }
}

#[derive(Clone, Copy)]
pub struct Progress {
    pub interval: Interval,
    pub speed: Speed,
    session_offset: i64,
    position: f64,
}

impl Progress {
    pub fn new(interval: Interval, offset: i64, speed: Speed) -> Self {
        let session_offset = interval.seek_offset(offset);
        Self {
            interval,
            speed,
            session_offset,
            position: session_offset as f64,
        }
    }

    pub fn offset(self) -> i64 {
        self.position as i64
    }

    pub fn prefill_seconds(self, configured_seconds: u16) -> f64 {
        let remaining = (self.interval.seconds() - self.session_offset) as f64 / self.speed.rate();
        (remaining - 0.1).clamp(0.0, f64::from(configured_seconds))
    }

    pub fn frame_position(self, elapsed: f64) -> f64 {
        self.session_offset as f64 + elapsed
    }

    pub fn update(&mut self, elapsed: f64) {
        if elapsed.is_finite() && elapsed >= 0.0 {
            // Decoder output can briefly be out of RTP timestamp order.
            self.position = self
                .position
                .max(self.session_offset as f64 + elapsed)
                .min(self.interval.seconds() as f64);
        }
    }
}

#[cfg(test)]
fn url(
    base: Url,
    brand: crate::Brand,
    channel: u32,
    start: &str,
    end: &str,
) -> Result<Url, &'static str> {
    Interval::new(start, end)?.url_at(base, brand, channel, 0)
}

fn url_for_times(
    base: Url,
    brand: crate::Brand,
    channel: u32,
    start: DateTime,
    end: DateTime,
) -> Result<Url, &'static str> {
    if channel == 0 {
        return Err("Canal inválido.");
    }
    let mut url = base;
    url.set_username("").map_err(|_| "URL inválida.")?;
    url.set_password(None).map_err(|_| "URL inválida.")?;
    url.set_query(None);
    match brand {
        crate::Brand::Intelbras => {
            url.set_path("/cam/playback");
            url.query_pairs_mut()
                .append_pair("channel", &channel.to_string())
                .append_pair(
                    "starttime",
                    &start.strftime("%Y_%m_%d_%H_%M_%S").to_string(),
                )
                .append_pair("endtime", &end.strftime("%Y_%m_%d_%H_%M_%S").to_string());
        }
        crate::Brand::Hikvision => {
            let track = channel
                .checked_mul(100)
                .and_then(|n| n.checked_add(1))
                .ok_or("Canal inválido.")?;
            url.set_path(&format!("/Streaming/tracks/{track}"));
            url.query_pairs_mut()
                .append_pair("starttime", &start.strftime("%Y%m%dT%H%M%SZ").to_string())
                .append_pair("endtime", &end.strftime("%Y%m%dT%H%M%SZ").to_string());
        }
        crate::Brand::Custom => {
            return Err("Reprodução não disponível para este tipo de equipamento.")
        }
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_defaults_use_local_date_and_convert_both_endpoints_to_utc() {
        let now = "2024-03-01T22:15:03.890-03:00[-03:00]".parse().unwrap();
        let times = OpeningTimes::new(now);
        assert_eq!(
            times.fields(false),
            ["2024-03-01 07:30:00", "2024-03-01 22:15:03"]
        );
        assert_eq!(
            times.fields(true),
            ["2024-03-01 10:30:00", "2024-03-02 01:15:03"]
        );
        assert_eq!(times.offset_field().as_deref(), Some("-03:00"));
    }

    #[test]
    fn early_opening_defaults_are_valid_including_at_midnight_and_at_seven_thirty() {
        for (now, start) in [
            ("00:00:00", "2024-02-29 23:59:59"),
            ("06:45:00", "2024-03-01 00:00:00"),
            ("07:30:00", "2024-03-01 00:00:00"),
            ("07:30:01", "2024-03-01 07:30:00"),
        ] {
            let now = format!("2024-03-01T{now}+00:00[UTC]").parse().unwrap();
            let [actual_start, end] = OpeningTimes::new(now).fields(false);
            assert_eq!(actual_start, start);
            assert!(Interval::new(&actual_start, &end).is_ok());
        }
    }

    #[test]
    fn sync_accepts_ten_recording_seconds_of_lag_without_showing_future_frames() {
        for speed in Speed::ALL {
            assert_eq!(frame_timing(20.0, 30.0, speed), FrameTiming::Present);
            assert_eq!(frame_timing(19.9, 30.0, speed), FrameTiming::Drop);
            assert_eq!(frame_timing(31.0, 30.0, speed), FrameTiming::Wait);
        }
    }

    #[test]
    fn sync_clock_waits_for_ready_cameras_without_waiting_forever() {
        let now = Instant::now();
        let mut clock = SyncClock::new(12, Speed::Double, 7.0, now);
        clock.tick(now + Duration::from_secs(2), false);
        assert_eq!(clock.position(now + Duration::from_secs(2)), 12.0);
        clock.tick(now + Duration::from_secs(3), true);
        assert_eq!(clock.position(now + Duration::from_secs(5)), 16.0);
        let mut clock = SyncClock::new(12, Speed::Normal, 20.0, now);
        clock.tick(now + Duration::from_secs(24), false);
        assert!(!clock.started());
        clock.tick(now + Duration::from_secs(25), false);
        assert!(clock.started());
    }

    #[test]
    fn prefill_respects_the_setting_and_short_remaining_intervals() {
        let interval = Interval::new("2024-03-01 10:00:00", "2024-03-01 10:01:00").unwrap();
        assert_eq!(
            Progress::new(interval, 0, Speed::Normal).prefill_seconds(7),
            7.0
        );
        assert_eq!(
            Progress::new(interval, 0, Speed::Normal).prefill_seconds(20),
            20.0
        );
        assert_eq!(
            Progress::new(interval, 56, Speed::Double).prefill_seconds(7),
            1.9
        );
    }

    #[test]
    fn mixed_group_converts_the_same_instant_to_dvr_local_time() {
        let minutes = utc_offset_minutes("-03:00").unwrap();
        let utc = Interval::new("2024-03-01 01:00:00", "2024-03-01 01:01:00").unwrap();
        let local = utc.shifted(minutes).unwrap();
        assert_eq!(local.label_at(15), "2024-02-29 22:00:15");
        assert_eq!(
            local.shifted(-minutes).unwrap().label_at(15),
            utc.label_at(15)
        );
        for invalid in ["", "03:00", "-03:60", "+14:01", "-15:00", "não"] {
            assert!(utc_offset_minutes(invalid).is_err());
        }
    }

    #[test]
    fn seek_crosses_midnight_and_keeps_original_end_for_both_brands() {
        let interval = Interval::new("2024-02-29 23:59:50", "2024-03-01 00:00:20").unwrap();
        assert_eq!(interval.seconds(), 30);
        assert_eq!(interval.label_at(15), "2024-03-01 00:00:05");
        assert_eq!(interval.seek_offset(30), 29);
        let base = Url::parse("rtsp://example.com/").unwrap();
        for (brand, start, end) in [
            (
                crate::Brand::Intelbras,
                "2024_03_01_00_00_05",
                "2024_03_01_00_00_20",
            ),
            (
                crate::Brand::Hikvision,
                "20240301T000005Z",
                "20240301T000020Z",
            ),
        ] {
            let url = interval.url_at(base.clone(), brand, 1, 15).unwrap();
            let params: std::collections::HashMap<_, _> = url.query_pairs().collect();
            assert_eq!(params.get("starttime").unwrap(), start);
            assert_eq!(params.get("endtime").unwrap(), end);
        }
    }

    #[test]
    fn speed_change_resumes_from_progress_and_keeps_interval() {
        let interval = Interval::new("2024-02-29 12:00:00", "2024-02-29 12:02:00").unwrap();
        let mut running = Progress::new(interval, 20, Speed::Normal);
        running.update(7.8);
        running.update(6.0);
        assert_eq!(running.offset(), 27);
        let mut resumed = Progress::new(running.interval, running.offset(), Speed::Double);
        assert_eq!(resumed.offset(), 27);
        resumed.update(8.0);
        assert_eq!(resumed.offset(), 35);
        resumed.update(200.0);
        assert_eq!(resumed.offset(), 120);
    }

    #[test]
    fn masks_partial_full_and_pasted_dates() {
        assert_eq!(mask_datetime("2024"), "2024");
        assert_eq!(mask_datetime("20240229123045"), "2024-02-29 12:30:45");
        assert_eq!(mask_datetime("2024-02-29 12:30:45"), "2024-02-29 12:30:45");
        assert_eq!(mask_datetime("2024022912304599"), "2024-02-29 12:30:45");
        assert_eq!(cursor_after_digits("2024-02-29", 6), 7);
    }

    #[test]
    fn builds_playback_url_without_credentials() {
        let base = Url::parse("rtsp://user:secret@example.com:554/").unwrap();
        let url = url(
            base,
            crate::Brand::Intelbras,
            2,
            "2024-02-29 12:00:00",
            "2024-03-01 01:02:03",
        )
        .unwrap();
        assert_eq!(url.as_str(), "rtsp://example.com:554/cam/playback?channel=2&starttime=2024_02_29_12_00_00&endtime=2024_03_01_01_02_03");
    }

    #[test]
    fn builds_hikvision_playback_url_with_utc_times_and_no_credentials() {
        let base = Url::parse("rtsp://user:secret@example.com:554/").unwrap();
        let result = url(
            base,
            crate::Brand::Hikvision,
            2,
            "2024-02-29 12:00:00",
            "2024-03-01 01:02:03",
        )
        .unwrap();
        assert_eq!(result.as_str(), "rtsp://example.com:554/Streaming/tracks/201?starttime=20240229T120000Z&endtime=20240301T010203Z");
    }

    #[test]
    fn rejects_invalid_dates_and_intervals() {
        let base = Url::parse("rtsp://example.com/").unwrap();
        for (start, end) in [
            ("2023-02-29 00:00:00", "2023-03-01 00:00:00"),
            ("2024-01-01 25:00:00", "2024-01-02 00:00:00"),
            ("2024-01-01 00:00:00", "2024-01-01 00:00:00"),
            ("2024-01-01 00:00:00", "2023-12-31 23:59:59"),
            ("2024-01-01T00:00:00", "2024-01-02 00:00:00"),
        ] {
            for brand in [crate::Brand::Intelbras, crate::Brand::Hikvision] {
                assert!(url(base.clone(), brand, 1, start, end).is_err());
            }
        }
        assert!(url(
            base.clone(),
            crate::Brand::Hikvision,
            0,
            "2024-01-01 00:00:00",
            "2024-01-02 00:00:00"
        )
        .is_err());
        assert!(url(
            base,
            crate::Brand::Hikvision,
            u32::MAX,
            "2024-01-01 00:00:00",
            "2024-01-02 00:00:00"
        )
        .is_err());
    }
}
