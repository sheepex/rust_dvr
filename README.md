# Visor DVR

Aplicativo desktop em Rust para cadastrar DVRs e NVRs e visualizar câmeras ao vivo por RTSP. O primeiro protótipo oferece modelos de URL para equipamentos Intelbras e Hikvision. Não usa SDK proprietário.

## Requisitos

- Rust e Cargo.
- No Linux, as bibliotecas de desenvolvimento necessárias à janela gráfica, como X11/Wayland e OpenGL, conforme a distribuição.
- DVR/NVR acessível pela rede, com RTSP habilitado e usuário autorizado. A porta RTSP costuma ser 554, mas pode ser diferente.

O projeto utiliza `eframe` para a interface, `retina` para RTSP e `rust_h264` para decodificação H.264 em Rust, sem FFmpeg. A intenção é suportar Linux, Windows e macOS; a compilação e o funcionamento ainda precisam ser verificados em cada plataforma. A interface gráfica e o armazenamento seguro continuam usando serviços e bibliotecas do sistema.

## Executar

```sh
cargo run --release
```

Na aba **Equipamentos**, aberta pelo menu, clique em **Adicionar**, preencha os dados, inclusive a senha, e salve. Na aba **Visualização ao vivo**, aberta por padrão, escolha na parte inferior da lista uma grade de 1, 4, 9 ou 16 quadros. Selecione um quadro, expanda o equipamento e dê um duplo clique na câmera. A seleção avança ao próximo quadro, permitindo abrir outras câmeras com duplo clique; após o último, retorna ao primeiro. Abrir uma câmera em um quadro ocupado substitui sua transmissão. Ao reduzir a grade, as câmeras dos quadros que deixam de ser exibidos são desconectadas. Para desconectar um quadro, clique nele com o botão direito e escolha **Desconectar**; na lista, a mesma opção fecha todas as exibições daquela câmera. A senha salva é carregada automaticamente e o fluxo secundário é utilizado. Para alterar os dados ou a senha, clique em **Editar** na lista de equipamentos.

No menu de contexto de um quadro ou de uma câmera, **Fechar todas as câmeras** desconecta todas as transmissões abertas.

Na aba **Reprodução**, selecione uma câmera Intelbras, um quadro e informe início e fim no formato `AAAA-MM-DD HH:MM:SS`, usando o horário configurado no DVR. Clique em **Reproduzir**. É possível abrir até quatro trechos e fechá-los pelo menu de contexto dos quadros. A reprodução ainda precisa de validação visual de horário e velocidade no equipamento; Hikvision não é suportada nessa aba. A consulta dos períodos gravados, linha do tempo, pausa e avanço ainda não estão implementados.

A porta cadastrada é a **porta RTSP**, normalmente 554; a porta 37777 de serviço de alguns Intelbras não é a porta RTSP. Consulte a configuração do equipamento para confirmar. Os diagnósticos são aproximados e não substituem o teste com o equipamento.

Os modelos usados são `/cam/realmonitor?channel=1&subtype=1` para Intelbras e `/Streaming/Channels/102` para Hikvision (canal 1, fluxo secundário). Os caminhos e a disponibilidade variam conforme o modelo e a configuração. Por enquanto somente H.264 é decodificado; H.265 ainda não está implementado.

## Privacidade e limitações

- Os dados do equipamento, incluindo o nome de usuário, são salvos em JSON na pasta de configuração do usuário. A senha é guardada separadamente no gerenciador de credenciais do sistema (Secret Service no Linux, Keychain no macOS e Credential Manager no Windows); o serviço precisa estar disponível e desbloqueado. Se não estiver, a senha não será salva em JSON. A senha permanece na memória enquanto estiver em uso, mas não é passada em argumentos de processo externo.
- RTSP normalmente não criptografa vídeo nem credenciais. Prefira rede local confiável ou VPN; não exponha o DVR diretamente à internet.
- A grade permite até 16 transmissões simultâneas, sujeitas ao desempenho do computador, da rede e do equipamento. Não há áudio, gravação, descoberta automática, exclusão de cadastro ou reconexão. Os canais listados são gerados pela quantidade informada, não detectados automaticamente.
- A ausência de imagem pode ser causada por caminho RTSP incompatível, rede, autenticação, codec ou RTSP desativado. H.265 não é suportado ainda e alguns perfis de H.264 podem ser incompatíveis.

## Testes

```sh
cargo test
```
