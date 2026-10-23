<p align="center">
  <img src="src-tauri/icons/icon.png" width="128" alt="Ícone do Meraki Flash">
</p>

<h1 align="center">Meraki Flash</h1>

<p align="center">Crie pendrives bootáveis do Windows e Linux, confira imagens e restaure dispositivos USB.</p>

<p align="center">
  <a href="https://github.com/merakkicode/MERAKI-FLASH/releases/latest"><img src="https://img.shields.io/github/v/release/merakkicode/MERAKI-FLASH?display_name=tag&amp;sort=date&amp;style=for-the-badge" alt="Versão mais recente"></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/merakkicode/MERAKI-FLASH?style=for-the-badge" alt="Licença MIT"></a>
  <img src="https://img.shields.io/badge/plataformas-Windows%20%7C%20Linux-00bcd4?style=for-the-badge" alt="Windows e Linux">
  <a href="https://github.com/merakkicode/MERAKI-FLASH/actions/workflows/release.yml"><img src="https://github.com/merakkicode/MERAKI-FLASH/actions/workflows/release.yml/badge.svg" alt="Verificação e publicação"></a>
</p>

Desenvolvido por **Guilherme Lindner**.

## Download

Baixe a **v1.0.0** somente pelos [lançamentos oficiais](https://github.com/merakkicode/MERAKI-FLASH/releases/latest). Disponível para computadores x86-64.

| Sistema | Formato | Download |
| --- | --- | --- |
| Linux | AppImage portátil | [AppImage v1.0.0](https://github.com/merakkicode/MERAKI-FLASH/releases/download/1.0.0/Meraki.Flash_1.0.0_amd64.AppImage) |
| Linux | RPM para Fedora, RHEL, openSUSE e derivados | [RPM v1.0.0](https://github.com/merakkicode/MERAKI-FLASH/releases/download/1.0.0/Meraki.Flash-1.0.0-1.x86_64.rpm) |
| Windows 10/11 | Instalador EXE | [Instalador v1.0.0](https://github.com/merakkicode/MERAKI-FLASH/releases/download/1.0.0/Meraki.Flash_1.0.0_x64-setup.exe) |

Consulte as [notas da versão](RELEASE_NOTES.md).

### Instalação

No Linux, conceda permissão ao AppImage e execute-o:

```bash
chmod +x Meraki.Flash*.AppImage
./Meraki.Flash*.AppImage
```

Para o RPM, use `sudo dnf install ./Meraki.Flash*.rpm` ou `sudo zypper install ./Meraki.Flash*.rpm`. A gravação exige `pkexec` e um agente de autenticação polkit ativo na sessão gráfica.

No Windows, execute o instalador `Meraki.Flash_*_x64-setup.exe` e abra o aplicativo pelo Menu Iniciar. O UAC solicita autorização quando uma gravação ou formatação precisa de acesso ao disco. O instalador ainda não possui assinatura digital.

## Como usar

1. Escolha se a imagem contém Windows ou Linux.
2. Selecione a imagem. Opcionalmente, calcule SHA-256 ou MD5 e compare com o valor fornecido pela distribuição.
3. Escolha o dispositivo USB.
4. Para Windows, configure as opções de instalação automatizada se desejar.
5. Clique em **Gravar pendrive**, confira nome, capacidade e caminho do destino e confirme.
6. Aguarde a conclusão. Para interromper, use **Cancelar gravação** e espere a liberação do dispositivo.

> [!CAUTION]
> Gravar ou restaurar um dispositivo apaga seus dados e partições. Confira o destino antes da confirmação. Uma operação cancelada pode deixar o dispositivo sem condições de inicialização; grave ou formate novamente antes de usá-lo.

## Recursos da versão 1.0.0

- Reconhecimento de pendrives e SSDs externos USB que reportam mídia fixa, preservando a exclusão de discos de sistema.
- Gravação Linux de `.iso`, `.img`, `.raw` e `.bin`, além de descompressão em streaming de `.gz`, `.xz` e `.zip`.
- Preparação Windows com MBR e FAT32 para UEFI, incluindo conversão de `install.esd` grande e divisão de WIM em arquivos SWM.
- Instalação Windows automatizada com opções de conta local, privacidade, BitLocker automático, instalação sem internet e nome do computador.
- Cancelamento cooperativo com sincronização e liberação dos recursos.
- Cálculo de SHA-256/MD5, progresso, cancelamento e comparação com hashes oficiais.
- Restauração de USB para uma partição FAT32 com rótulo personalizado, sem exigir uma imagem.
- Progresso em tempo real, verificação após gravação Linux e aviso opcional de novas versões.

## Formatos e motores

### Linux e imagens raw

As imagens são gravadas bit a bit em blocos de até 4 MiB. O cabeçalho inicial do conteúdo descompactado é validado antes da escrita. Os formatos GZ, XZ e ZIP são lidos em streaming, sem extrair a imagem inteira para o SSD do computador. O ZIP deve conter uma imagem compatível; arquivos adicionais não são extraídos para o disco do computador.

O tamanho é conferido quando disponível, e a escrita nunca pode ultrapassar a capacidade do destino. A verificação reabre a origem, inclusive o descompressor, e compara o conteúdo com os bytes gravados. Imagens comprimidas corrompidas ou truncadas são rejeitadas.

### Windows e UEFI

O modo Windows exige `.iso` ou `.img` não compactado com estrutura ISO9660/UDF apropriada e arquivos de inicialização UEFI. Não depende de executáveis como DISM, `diskpart` ou `mkfs`.

Quando `sources/install.wim` excede o limite FAT32, o aplicativo o divide em `install.swm`, `install2.swm` e partes subsequentes. Para `install.esd` grande, exporta todos os índices para um WIM intermediário não sólido e depois o divide. O conjunto SWM substitui o arquivo original em `sources/`.

No Windows, a conversão usa a WIMGAPI do sistema; no Linux, a wimlib incluída no aplicativo. A preparação verifica espaço temporário para a origem, o intermediário, as partes finais e uma margem. Arquivos temporários são removidos ao concluir, cancelar ou falhar. A preparação das partes ocorre antes de alterar o USB.

O destino recebe MBR e FAT32, com alinhamento de 1 MiB e tamanho de cluster escolhido conforme a geometria. Outros arquivos individuais maiores que 4 GiB menos 1 byte continuam incompatíveis com FAT32. O limite conservador de dispositivo é 2 TiB.

### Instalação automatizada

As opções de `autounattend.xml` incluem:

- ignorar verificações de TPM, Secure Boot e RAM;
- criar conta local e reduzir perguntas de privacidade;
- desativar o Edge em segundo plano, ocultar a pesquisa e remover pacotes conhecidos do Copilot;
- impedir a criptografia automática do dispositivo;
- configurar BypassNRO para instalação sem internet;
- definir nome de computador válido de até 15 caracteres.

As opções para impedir criptografia automática e permitir instalação sem internet começam ativadas quando o painel de instalação automatizada é usado. A aplicação dessas configurações depende da edição e versão do instalador Windows; mudanças futuras da Microsoft podem afetar os mecanismos de OOBE. A conta local criada não tem senha inicial: defina uma senha segura no primeiro acesso.

O XML é validado no frontend e no helper, limitado a 16 KiB e rejeitado se contiver DTD/DOCTYPE ou estrutura inválida.

### Hash e integridade

A Etapa 02 calcula SHA-256 ou MD5 em segundo plano. É possível colar apenas o hash ou uma linha de `SHA256SUMS`, `sha256sum` ou formato BSD. A comparação ignora diferenças de maiúsculas/minúsculas. Trocar a imagem cancela o cálculo anterior e limpa o resultado.

Use hashes obtidos no site oficial por uma conexão confiável. A coincidência comprova que os bytes correspondem ao hash informado; por si só, não comprova a origem desse hash. Prefira SHA-256; MD5 está disponível para compatibilidade com verificações legadas.

### Restaurar / Formatar pendrive

Na Etapa 03, selecione um USB e abra **Restaurar / Formatar pendrive**. Confira o destino e informe um rótulo de 1 a 11 letras ASCII ou números. O padrão é `PENDRIVE`.

A restauração limpa os primeiros e os últimos 2 MiB, cria uma partição MBR ativa do tipo FAT32 LBA e formata sua capacidade disponível. São reservados 1 MiB em cada extremidade para alinhamento, além dos metadados FAT32. Ao finalizar, o aplicativo sincroniza, libera os volumes e solicita a releitura da tabela ao sistema operacional.

É uma formatação rápida, não um apagamento seguro de todos os dados. FAT32 aceita arquivos de até 4 GiB menos 1 byte. O cancelamento é cooperativo e aguarda operações de I/O já pendentes.

## Proteções

A descoberta aceita discos físicos USB, independentemente do bit removível. Discos internos e de sistema/boot são excluídos. No Linux, a análise percorre os volumes descendentes e rejeita montagens críticas e swap; no Windows, verifica os indicadores de sistema/boot e a unidade do sistema. Metadados insuficientes são tratados de forma restritiva.

O helper elevado redescobre o hardware e compara sua identidade antes de abrir o destino. Dispositivos somente-leitura e imagens armazenadas no próprio destino são rejeitados. Os volumes são desmontados no Linux e bloqueados/desmontados no Windows. O canal local de progresso e controle é autenticado; sua desconexão solicita cancelamento.

A sincronização e a liberação de recursos precedem a notificação terminal à interface. Falhas de sincronização ou atualização de partições são apresentadas ao usuário.

## Desenvolvimento

O projeto usa Tauri 2, Rust, React 19, TypeScript e Tailwind CSS 4.

Requer Node.js 22, Rust 1.95 ou mais recente e as dependências nativas do Tauri. No Linux, também são necessários compilador C, `make`, Autotools e `libclang` para compilar a wimlib incluída.

```bash
git clone https://github.com/merakkicode/MERAKI-FLASH.git
cd MERAKI-FLASH
npm ci
npm run tauri dev
```

### Testes e compilação

```bash
npm test
npm run build
cargo test --locked --workspace --manifest-path src-tauri/Cargo.toml
cargo clippy --locked --workspace --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
```

Os testes usam arquivos temporários e imagens sintéticas para validar MBR/FAT32, streaming, conversão ESD, cancelamento, hashes e filtros de dispositivos. Não escrevem em USB físico. Testes de inicialização e compatibilidade com hardware exigem uma bancada dedicada com dados descartáveis.

### Organização

| Local | Responsabilidade |
| --- | --- |
| `src/App.tsx` | Fluxo da interface, progresso e cancelamento |
| `src/components/` | Painéis de instalação, hash e restauração |
| `src/lib/` | Validações e estado da interface |
| `src-tauri/src/commands/` | Comandos Tauri de USB, gravação, formatação e hash |
| `src-tauri/src/flash/` | Helper, streaming, ISO/UDF, WIM/ESD e FAT32 |
| `src-tauri/crates/meraki-flash-core/` | Descoberta segura e validação de imagens |
| `src-tauri/crates/wimlib-sys/` | Binding e fonte da wimlib para Linux |

### Lançamentos

O workflow [release.yml](.github/workflows/release.yml) testa Windows e Linux. Tags no formato `X.Y.Z` ou `vX.Y.Z` devem corresponder à versão dos manifestos. A versão desta conta usa a tag `1.0.0`, preservando a tag histórica `v1.0.0` da conta de origem. A publicação exige testes, Clippy e os três instaladores: EXE, AppImage e RPM. A release permanece em rascunho enquanto os builds não terminam com sucesso.

O aviso opcional de atualizações consulta a última release estável ao abrir, sem baixar nem instalar automaticamente. Pode ser desativado na interface. Usuários da v0.1.0 precisam instalar uma versão recente manualmente para obter esse aviso.

## Licença

Copyright © 2026 Guilherme Lindner. Distribuído sob a [licença MIT](LICENSE).

No Linux, a wimlib incluída é compilada sem `ntfs-3g`, sob a opção LGPL-3.0+ oferecida pelo projeto upstream. Ao redistribuir binários modificados, observe as obrigações da LGPL e o aviso do binding em `src-tauri/crates/wimlib-sys`.
