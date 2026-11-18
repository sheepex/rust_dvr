# Alterações locais do Visor DVR

Cópia do Retina 0.4.22 (upstream `scottlamb/retina`, commit
`2b6b7fe0a6f6a5bf69d5e9c8d4fd785ccd090873`).

Em `src/client/mod.rs`, `Session::play_with_timing` permite enviar o cabeçalho
RTSP `Scale` no `PLAY` inicial e ler `Scale` e `Range` da resposta do servidor.
`Session::play_with_scale` preserva a interface anterior, e `Session::play`
continua sem enviar `Scale`. A reprodução em grupo usa `Range: clock=…`
como referência de horário quando o DVR o fornece.
