use url::Url;

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

fn parse_datetime(input: &str) -> Result<String, &'static str> {
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
    let number = |start, end| input[start..end].parse::<u32>().unwrap();
    let (year, month, day) = (number(0, 4), number(5, 7), number(8, 10));
    let (hour, minute, second) = (number(11, 13), number(14, 16), number(17, 19));
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => 0,
    };
    if year == 0 || day == 0 || day > days || hour > 23 || minute > 59 || second > 59 {
        return Err("Data ou hora inválida.");
    }
    Ok(input.replace(['-', ' ', ':'], "_"))
}

pub fn url(
    base: Url,
    brand: crate::Brand,
    channel: u32,
    start: &str,
    end: &str,
) -> Result<Url, &'static str> {
    if channel == 0 {
        return Err("Canal inválido.");
    }
    let start = parse_datetime(start)?;
    let end = parse_datetime(end)?;
    if start >= end {
        return Err("O horário final deve ser posterior ao inicial.");
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
                .append_pair("starttime", &start)
                .append_pair("endtime", &end);
        }
        crate::Brand::Hikvision => {
            let track = channel
                .checked_mul(100)
                .and_then(|n| n.checked_add(1))
                .ok_or("Canal inválido.")?;
            let hik_time = |time: &str| {
                format!(
                    "{}T{}Z",
                    time[..10].replace('_', ""),
                    time[11..].replace('_', "")
                )
            };
            url.set_path(&format!("/Streaming/tracks/{track}"));
            url.query_pairs_mut()
                .append_pair("starttime", &hik_time(&start))
                .append_pair("endtime", &hik_time(&end));
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
