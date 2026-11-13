# Visor DVR

Aplicativo desktop em Rust para cadastrar DVRs e NVRs e visualizar uma câmera ao vivo por RTSP. O primeiro protótipo oferece modelos de URL para equipamentos Intelbras e Hikvision. Não usa SDK proprietário.

## Requisitos

- Rust e Cargo.
- No Linux, as bibliotecas de desenvolvimento necessárias à janela gráfica, como X11/Wayland e OpenGL, conforme a distribuição.
- DVR/NVR acessível pela rede, com RTSP habilitado e usuário autorizado. A porta RTSP costuma ser 554, mas pode ser diferente.

O projeto utiliza `eframe` para a interface, `retina` para RTSP e `rust_h264` para decodificação H.264 em Rust, sem FFmpeg. A intenção é suportar Linux, Windows e macOS; a compilação e o funcionamento ainda precisam ser verificados em cada plataforma. A interface gráfica e o armazenamento seguro continuam usando serviços e bibliotecas do sistema.

## Executar

```sh
cargo run --release
```

Na aba **Equipamentos**, clique em **Adicionar**, preencha os dados, inclusive a senha, e salve. Abra a aba **Visualização ao vivo** pelo menu, expanda o equipamento na lista, escolha uma câmera e um quadro da grade e clique em **Visualizar câmera**. A senha salva é carregada automaticamente; também é possível informá-la e clicar em **Salvar senha** na visualização. Experimente o fluxo secundário para reduzir o consumo de rede. Para editar um cadastro, clique em **Editar** na lista.

A porta cadastrada é a **porta RTSP**, normalmente 554; a porta 37777 de serviço de alguns Intelbras não é a porta RTSP. Consulte a configuração do equipamento para confirmar. Os diagnósticos são aproximados e não substituem o teste com o equipamento.

Os modelos usados são `/cam/realmonitor?channel=1&subtype=1` para Intelbras e `/Streaming/Channels/102` para Hikvision (canal 1, fluxo secundário). Os caminhos e a disponibilidade variam conforme o modelo e a configuração. Por enquanto somente H.264 é decodificado; H.265 ainda não está implementado.

## Privacidade e limitações

- Os dados do equipamento, incluindo o nome de usuário, são salvos em JSON na pasta de configuração do usuário. A senha é guardada separadamente no gerenciador de credenciais do sistema (Secret Service no Linux, Keychain no macOS e Credential Manager no Windows); o serviço precisa estar disponível e desbloqueado. Se não estiver, a senha não será salva em JSON. A senha permanece na memória enquanto estiver em uso, mas não é passada em argumentos de processo externo.
- RTSP normalmente não criptografa vídeo nem credenciais. Prefira rede local confiável ou VPN; não exponha o DVR diretamente à internet.
- Este protótipo exibe apenas um canal por vez, sem áudio, gravação, descoberta automática, exclusão de cadastro ou reconexão. Os canais listados são gerados pela quantidade informada, não detectados automaticamente.
- A ausência de imagem pode ser causada por caminho RTSP incompatível, rede, autenticação, codec ou RTSP desativado. H.265 não é suportado ainda e alguns perfis de H.264 podem ser incompatíveis.

## Testes

```sh
cargo test
```
