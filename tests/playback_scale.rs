use retina::client::{PlayOptions, Session, SessionOptions, SetupOptions, TeardownPolicy};
use std::{
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    time::Duration,
};

#[tokio::test]
async fn sends_scale_only_when_requested_and_reads_dvr_timing() {
    for (requested, reply, expected) in [
        (None, "", None),
        (Some(0.5), "Scale: 0.5\r\n", Some(0.5)),
        (Some(4.0), "Scale: 2.0\r\n", Some(2.0)),
        (Some(2.0), "", None),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("rtsp://{}/recording", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut socket = BufReader::new(socket);
            for method in ["DESCRIBE", "SETUP", "PLAY"] {
                let mut line = String::new();
                assert!(socket.read_line(&mut line).unwrap() > 0);
                assert!(line.starts_with(&format!("{method} ")), "{line}");
                let mut cseq = None;
                let mut scale = None;
                loop {
                    line.clear();
                    assert!(socket.read_line(&mut line).unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                    let (key, value) = line.split_once(':').unwrap();
                    if key.eq_ignore_ascii_case("cseq") {
                        cseq = Some(value.trim().to_owned());
                    } else if key.eq_ignore_ascii_case("scale") {
                        scale = Some(value.trim().parse::<f64>().unwrap());
                    }
                }
                assert_eq!(scale, if method == "PLAY" { requested } else { None });
                let cseq = cseq.unwrap();
                let response = match method {
                    "DESCRIBE" => {
                        let sdp = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=Test\r\nt=0 0\r\na=control:*\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=control:track1\r\n";
                        format!(
                            "Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{sdp}",
                            sdp.len()
                        )
                    }
                    "SETUP" => {
                        "Session: test\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
                            .into()
                    }
                    "PLAY" => {
                        format!("Session: test\r\nRange: clock=20240301T000000Z-\r\n{reply}\r\n")
                    }
                    _ => unreachable!(),
                };
                write!(
                    socket.get_mut(),
                    "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\n{response}"
                )
                .unwrap();
            }
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut session = Session::describe(
                url.parse().unwrap(),
                SessionOptions::default().teardown(TeardownPolicy::Never),
            )
            .await
            .unwrap();
            session.setup(0, SetupOptions::default()).await.unwrap();
            let (_, reported, range) = session
                .play_with_timing(PlayOptions::default(), requested)
                .await
                .unwrap();
            assert_eq!(reported, expected);
            assert_eq!(range.as_deref(), Some("clock=20240301T000000Z-"));
        })
        .await
        .unwrap();
        server.join().unwrap();
    }
}
