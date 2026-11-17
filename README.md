# Visor DVR

Aplicativo desktop em Rust para cadastrar DVRs e NVRs e visualizar câmeras ao vivo por RTSP. O primeiro protótipo oferece modelos de URL para equipamentos Intelbras e Hikvision. Não usa SDK proprietário.

## Requisitos

- Rust e Cargo.
- No Linux, as bibliotecas de desenvolvimento necessárias à janela gráfica, como X11/Wayland e OpenGL, conforme a distribuição.
- DVR/NVR acessível pela rede, com RTSP habilitado e usuário autorizado. A porta RTSP costuma ser 554, mas pode ser diferente.

O projeto utiliza `eframe` para a interface, `retina` para RTSP e as bibliotecas Rust `rust_h264` e `rust_h265` para decodificação, sem FFmpeg instalado. A intenção é suportar Linux, Windows e macOS; a compilação e o funcionamento ainda precisam ser verificados em cada plataforma. A interface gráfica e o armazenamento seguro continuam usando serviços e bibliotecas do sistema.

## Executar

```sh
cargo run --release
```

### Integração com o Linux

O diretório `packaging/linux` contém o atalho e o ícone do aplicativo. Para uma instalação local, compile com `cargo build --release --bin visor-dvr`, instale o executável em `~/.local/bin/visor-dvr`, o ícone em `~/.local/share/icons/hicolor/scalable/apps/visor-dvr.svg` e o atalho em `~/.local/share/applications/visor-dvr.desktop`. O campo `Exec` do atalho deve apontar para o executável instalado; use um caminho absoluto se `~/.local/bin` não estiver no `PATH` da sessão gráfica. O identificador `visor-dvr` deve ser mantido no nome do atalho, no ícone, em `StartupWMClass` e na janela para associá-los no GNOME.

Na aba **Equipamentos**, aberta pelo menu, clique em **Adicionar**, preencha os dados, inclusive a senha, e salve. Na aba **Visualização ao vivo**, aberta por padrão, escolha na parte inferior da lista uma grade de 4, 9 ou 16 quadros. Selecione um quadro, expanda o equipamento e dê um duplo clique na câmera. A seleção avança ao próximo quadro, permitindo abrir outras câmeras com duplo clique; após o último, retorna ao primeiro. Dê um duplo clique no quadro para ampliá-lo e outro para retornar à grade anterior sem desconectar os demais. Abrir uma câmera em um quadro ocupado substitui sua transmissão. Ao reduzir a grade, as câmeras dos quadros que deixam de ser exibidos são desconectadas. Para desconectar um quadro, clique nele com o botão direito e escolha **Desconectar**; na lista, a mesma opção fecha todas as exibições daquela câmera. A senha salva é carregada automaticamente. Para alterar os dados ou a senha, clique em **Editar** na lista de equipamentos.

A qualidade é automática: na grade de 4 quadros usa-se o fluxo principal; nas grades de 9 e 16, o secundário para economizar rede e processamento. Ao ampliar um quadro, ele passa ao fluxo principal; os demais continuam no fluxo correspondente à grade. Ao restaurar ou mudar a grade, as transmissões necessárias são reconectadas, o que pode causar uma breve interrupção. A resolução efetiva depende do DVR. Fluxos H.264 e H.265 são suportados, sujeitos às limitações dos decodificadores.

Para ajustar uma imagem, clique com o botão direito no quadro e abra **Redimensionar imagem**. É possível alterar largura e altura independentemente, preencher o quadro ou restaurar a proporção original. Valores maiores podem cortar as bordas. O ajuste é visual, pertence ao quadro durante a sessão e também está disponível na Reprodução; não modifica a câmera nem o arquivo exportado.

No menu de contexto de um quadro ou de uma câmera, **Fechar todas as câmeras** desconecta todas as transmissões abertas.

Na aba **Reprodução**, selecione uma câmera Intelbras ou Hikvision, um quadro e informe início e fim no formato `AAAA-MM-DD HH:MM:SS`. Para Intelbras, use o horário configurado no DVR; para Hikvision, informe os horários em **UTC**, convertendo-os a partir do fuso do equipamento. Clique em **Reproduzir**. É possível abrir até quatro trechos e fechá-los pelo menu de contexto dos quadros. A reprodução Hikvision ainda precisa ser validada em equipamentos reais; caminhos, faixas de horário e permissões podem variar conforme o modelo. A consulta dos períodos gravados, linha do tempo, pausa e avanço ainda não estão implementados.

Nos campos de data e hora, digite os 14 números em sequência; os separadores são inseridos automaticamente. Por exemplo, `20241116153000` aparece como `2024-11-16 15:30:00`.

Para salvar, selecione uma câmera e informe o intervalo, depois clique em **Exportar intervalo…** e escolha um novo nome de arquivo `.mkv`. O vídeo é escrito progressivamente em disco, sem FFmpeg externo e sem áudio; mantenha o aplicativo aberto enquanto a exportação estiver em andamento. Arquivos existentes não são substituídos. Caso ocorra erro ou o aplicativo seja fechado, o arquivo criado pode estar incompleto. Mesmo quando o fluxo termina, a cobertura do intervalo não é confirmada: pode haver lacunas ou trechos ausentes, especialmente em gravações por movimento. Confira o arquivo e os horários antes de depender dele. A exportação precisa ser validada em equipamentos reais e pode não funcionar com todos os modelos ou formatos.

A porta cadastrada é a **porta RTSP**, normalmente 554; a porta 37777 de serviço de alguns Intelbras não é a porta RTSP. Consulte a configuração do equipamento para confirmar. Os diagnósticos são aproximados e não substituem o teste com o equipamento.

Os modelos usados para o canal 1 são `/cam/realmonitor?channel=1&subtype=0` (principal) ou `subtype=1` (secundário) para Intelbras, e `/Streaming/Channels/101` (principal) ou `/Streaming/Channels/102` (secundário) para Hikvision. Os caminhos e a disponibilidade variam conforme o modelo e a configuração.

Na visualização ao vivo Hikvision H.265, o aplicativo pode decifrar o formato de criptografia de fluxo testado quando o código de verificação é igual à senha de acesso salva. O mesmo tratamento é usado na reprodução H.265, mas ainda não foi validado com gravações. Códigos distintos ainda não podem ser cadastrados separadamente; outros formatos de criptografia e H.264 criptografado não foram implementados. A compatibilidade pode variar entre modelos.

Ao iniciar, o aplicativo verifica os equipamentos em segundo plano e tenta consultar canais e nomes por HTTP, usando a porta HTTP cadastrada (padrão 80) e autenticação Digest. Verde indica conexão aceita, vermelho indica falha, e cinza indica verificação em andamento ou resultado inconclusivo; passe o mouse sobre o nome para detalhes. Quando a consulta de nomes não é suportada, mantém-se a quantidade manual. A lista consultada pode incluir canais configurados sem câmera conectada; ela não confirma o estado individual de cada câmera. Use **Verificar novamente** na aba Equipamentos para atualizar. Não é enviado Basic por HTTP sem proteção.

## Privacidade e limitações

- Os dados do equipamento, incluindo o nome de usuário, são salvos em JSON na pasta de configuração do usuário. A senha é guardada separadamente no gerenciador de credenciais do sistema (Secret Service no Linux, Keychain no macOS e Credential Manager no Windows); o serviço precisa estar disponível e desbloqueado. Se não estiver, a senha não será salva em JSON. A senha permanece na memória enquanto estiver em uso, mas não é passada em argumentos de processo externo.
- RTSP normalmente não criptografa vídeo nem credenciais. Prefira rede local confiável ou VPN; não exponha o DVR diretamente à internet.
- A grade permite até 16 transmissões simultâneas, sujeitas ao desempenho do computador, da rede e do equipamento. Não há áudio, gravação, descoberta automática, exclusão de cadastro ou reconexão. Os canais listados são gerados pela quantidade informada, não detectados automaticamente.
- A ausência de imagem pode ser causada por caminho RTSP incompatível, rede, autenticação, codec ou RTSP desativado. Alguns perfis e recursos de H.264/H.265 podem ser incompatíveis. O decodificador H.265 documenta suporte aos perfis Main e Main 10, mas não a todos os formatos e extensões, como 4:2:2 e screen content coding.

## Testes

```sh
cargo test
```
