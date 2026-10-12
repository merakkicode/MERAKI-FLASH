# Meraki Flash v2.0.0

Desenvolvido por **Guilherme Lindner**. Disponível para Windows 10/11 x64 (EXE) e Linux x86-64 (AppImage e RPM).

## Novidades

- **Mais dispositivos USB:** detecta pendrives e SSDs externos que se identificam como mídia fixa, mantendo as proteções contra seleção de discos internos e do sistema.
- **Mais formatos:** grava imagens Linux `.iso`, `.img`, `.raw` e `.bin`, com descompressão em streaming de `.gz`, `.xz` e `.zip`, sem extrair a imagem inteira para o SSD.
- **Windows com ESD grande:** exporta os índices de `install.esd` maior que 4 GiB para WIM não sólido e gera partes SWM compatíveis com FAT32. Mantém a divisão de WIM e a limpeza dos temporários.
- **Instalação automatizada ampliada:** opções para impedir BitLocker automático, configurar instalação sem internet e personalizar o nome do computador.
- **Cancelamento:** interrompe gravação ou formatação de forma cooperativa, sincroniza os buffers e libera os recursos antes de desbloquear a interface.
- **Validação de integridade:** calcula SHA-256 e MD5 com progresso e cancelamento, permite copiar o resultado e comparar hashes oficiais colados em diferentes formatos.
- **Restaurar / Formatar pendrive:** cria uma partição MBR/FAT32 com rótulo personalizado para voltar a usar o dispositivo normalmente, sem precisar selecionar uma imagem.

## Segurança e compatibilidade

A confirmação mostra o destino antes de qualquer operação destrutiva. O helper elevado revalida o hardware, rejeita dispositivos somente-leitura e reutiliza o canal autenticado para progresso e cancelamento. O motor mantém escritas alinhadas a setores de 512 ou 4096 bytes e verificação por releitura nas imagens Linux.

O modo Windows exige ISO/IMG não compactada com estrutura de instalação apropriada. FAT32 mantém o limite de 4 GiB menos 1 byte por arquivo; WIM/ESD grandes são tratados por divisão em SWM. A formatação rápida reserva espaço para alinhamento e metadados e não equivale a apagar todos os dados com segurança.

As configurações de instalação automatizada dependem da edição e versão do Windows. Após cancelar uma gravação ou formatação, repita a operação antes de usar o dispositivo. Hash coincidente confirma correspondência com o valor informado; obtenha esse valor de uma fonte oficial confiável.

## Instalação e atualização

Baixe o instalador correspondente ao seu sistema nos arquivos desta release. Usuários de versões com aviso de atualização receberão a indicação da v2.0.0; a instalação continua manual. Quem usa a v0.1.0 deve baixar esta versão diretamente.

Os testes automatizados usam arquivos temporários e dispositivos simulados; não substituem testes de inicialização em hardware físico. O instalador Windows permanece sem assinatura digital.
