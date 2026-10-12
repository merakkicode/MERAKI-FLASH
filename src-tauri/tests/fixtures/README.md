# Fixture ESD sólida

`install-solid.esd` contém somente dados sintéticos, sem arquivos Microsoft.
Gerado com wimlib 1.14.4, LZMS sólido, dois índices (`Edition One` e `Edition Two`).
Cada índice tem três arquivos de 2 MiB compartilhados e `edition.txt` específico.
O arquivo é pequeno por compressão; o conteúdo expandido permite testar SWMs
com limite de 3.000.000 bytes sem criar imagens de vários GiB.

Para reproduzir semanticamente (timestamps/GUID podem mudar):

1. Crie `payload/part-0.bin`, `part-1.bin`, `part-2.bin`, cada um com 2.097.152
   bytes. O byte no offset `n` do arquivo `i` vale `(n + i) % 251`.
2. Escreva `edition-one` (UTF-8, sem newline) em `payload/edition.txt`.
3. Execute `wimlib-imagex capture payload install-solid.esd "Edition One" --compress=LZMS --solid --no-acls`.
4. Substitua o texto por `edition-two`.
5. Execute `wimlib-imagex append payload install-solid.esd "Edition Two" --solid --no-acls`.
6. Execute `wimlib-imagex optimize install-solid.esd --solid --recompress`.

A ferramenta de geração não é dependência do aplicativo nem da suíte: os testes
incluem a fixture e usam WIMGAPI no Windows ou a wimlib embutida no Linux.
