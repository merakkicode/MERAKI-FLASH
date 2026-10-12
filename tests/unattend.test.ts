import test from "node:test";
import assert from "node:assert/strict";
import {
  DEFAULT_UNATTEND_OPTIONS,
  generateAutounattendXml,
  validateComputerName,
  validateUnattendOptions,
  type UnattendOptions,
} from "../src/lib/unattend.ts";

const NO_OPTIONS: UnattendOptions = {
  bypassWindows11Requirements: false,
  createLocalAccount: false,
  username: "",
  skipPrivacyQuestions: false,
  disableEdgeBackground: false,
  hideTaskbarSearch: false,
  removeCopilot: false,
  disableBitLocker: false,
  bypassNetworkRequirement: false,
  setComputerName: false,
  computerName: "",
};

function settings(xml: string, pass: string): string {
  return xml.match(new RegExp(`<settings pass="${pass}">([\\s\\S]*?)</settings>`))?.[1] ?? "";
}

test("novas opções têm os padrões solicitados", () => {
  assert.equal(DEFAULT_UNATTEND_OPTIONS.disableBitLocker, true);
  assert.equal(DEFAULT_UNATTEND_OPTIONS.bypassNetworkRequirement, true);
  assert.equal(DEFAULT_UNATTEND_OPTIONS.setComputerName, false);
  assert.equal(DEFAULT_UNATTEND_OPTIONS.computerName, "");
  assert.equal(validateUnattendOptions(DEFAULT_UNATTEND_OPTIONS), null);
});

test("nome do computador aceita ASCII e hífens internos, de 1 a 15 caracteres", () => {
  for (const name of ["A", "PC-TRABALHO", "pc-01", "1-PC", "123-456", "A".repeat(15), " PC-01 "]) {
    assert.equal(validateComputerName(name), null, name);
  }
});

test("nome do computador rejeita vazio, excesso, hífens nas pontas e apenas números", () => {
  for (const name of ["", "   ", "A".repeat(16), "-PC", "PC-", "-", "0", "123456789012345"]) {
    assert.equal(typeof validateComputerName(name), "string", name);
  }
});

test("nome do computador rejeita espaços internos, controles e caracteres especiais", () => {
  for (const character of [...' \\/:*?"<>|.,@#$%^&+=[]{}~_é😀', "\0", "\n", "\t"]) {
    assert.notEqual(validateComputerName(`PC${character}01`), null, JSON.stringify(character));
  }
});

test("valida o nome somente quando ativo e impede gerar XML inválido", () => {
  assert.equal(validateUnattendOptions({ ...DEFAULT_UNATTEND_OPTIONS, computerName: "<invalid>" }), null);
  for (const computerName of ["", "123", "PC&01", "<script>", "A".repeat(16)]) {
    const options = { ...DEFAULT_UNATTEND_OPTIONS, setComputerName: true, computerName };
    assert.equal(validateUnattendOptions(options), validateComputerName(computerName));
    assert.throws(() => generateAutounattendXml(options));
  }
  assert.notEqual(validateUnattendOptions({
    ...DEFAULT_UNATTEND_OPTIONS, createLocalAccount: true, username: "",
    setComputerName: true, computerName: "PC-01",
  }), null);
});

test("cada nova opção funciona sozinha e nenhuma seleção continua inválida", () => {
  assert.notEqual(validateUnattendOptions(NO_OPTIONS), null);
  assert.throws(() => generateAutounattendXml(NO_OPTIONS));
  for (const key of ["disableBitLocker", "bypassNetworkRequirement", "setComputerName"] as const) {
    const options = { ...NO_OPTIONS, [key]: true, computerName: "PC-01" };
    assert.equal(validateUnattendOptions(options), null, key);
    assert.match(generateAutounattendXml(options), /<settings pass=/);
  }
});

test("BitLocker usa Deployment no specialize e não gera comandos em outros passos", () => {
  const xml = generateAutounattendXml({ ...NO_OPTIONS, disableBitLocker: true });
  const specialize = settings(xml, "specialize");
  assert.match(specialize, /component name="Microsoft-Windows-Deployment"/);
  assert.ok(specialize.includes(
    "<Path>reg.exe add HKLM\\SYSTEM\\CurrentControlSet\\Control\\BitLocker /v PreventDeviceEncryption /t REG_DWORD /d 1 /f</Path>",
  ));
  assert.equal(settings(xml, "windowsPE"), "");
  assert.equal(settings(xml, "oobeSystem"), "");
});

test("BypassNRO é gerado no windowsPE e persiste no specialize", () => {
  const xml = generateAutounattendXml({ ...NO_OPTIONS, bypassNetworkRequirement: true });
  const pe = settings(xml, "windowsPE");
  const specialize = settings(xml, "specialize");
  assert.match(pe, /component name="Microsoft-Windows-Setup"/);
  assert.ok(pe.includes("<Path>reg.exe add HKLM\\SYSTEM\\Setup\\LabConfig /v BypassNRO /t REG_DWORD /d 1 /f</Path>"));
  assert.match(specialize, /component name="Microsoft-Windows-Deployment"/);
  assert.ok(specialize.includes("<Path>reg.exe add HKLM\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\OOBE /v BypassNRO /t REG_DWORD /d 1 /f</Path>"));
  assert.equal((xml.match(/\/v BypassNRO /g) ?? []).length, 2);
  assert.doesNotMatch(xml, /BypassTPMCheck|PreventDeviceEncryption/);
});

test("ComputerName fica no Shell-Setup de um único specialize, com atributos e trim", () => {
  for (const base of [NO_OPTIONS, DEFAULT_UNATTEND_OPTIONS]) {
    const xml = generateAutounattendXml({ ...base, setComputerName: true, computerName: " PC-TRABALHO " });
    const specialize = settings(xml, "specialize");
    assert.equal((xml.match(/<settings pass="specialize">/g) ?? []).length, 1);
    assert.match(specialize, /<component name="Microsoft-Windows-Shell-Setup" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">\s*<ComputerName>PC-TRABALHO<\/ComputerName>\s*<\/component>/);
    assert.doesNotMatch(settings(xml, "windowsPE") + settings(xml, "oobeSystem"), /ComputerName/);
    if (base === NO_OPTIONS) assert.doesNotMatch(xml, /Microsoft-Windows-Deployment|RunSynchronous/);
  }
});

test("desativar as opções remove exclusivamente seus comandos e tags", () => {
  const all = { ...DEFAULT_UNATTEND_OPTIONS, setComputerName: true, computerName: "PC-01" };
  assert.doesNotMatch(generateAutounattendXml({ ...all, disableBitLocker: false }), /BitLocker|PreventDeviceEncryption/);
  assert.doesNotMatch(generateAutounattendXml({ ...all, bypassNetworkRequirement: false }), /BypassNRO/);
  assert.doesNotMatch(generateAutounattendXml({ ...all, setComputerName: false }), /ComputerName/);
  const xml = generateAutounattendXml({ ...NO_OPTIONS, bypassWindows11Requirements: true });
  assert.doesNotMatch(xml, /PreventDeviceEncryption|BypassNRO|ComputerName|pass="specialize"/);
  assert.match(xml, /BypassTPMCheck/);
});

test("mantém escape XML em nomes permitidos e comandos com aspas", () => {
  const xml = generateAutounattendXml({
    ...DEFAULT_UNATTEND_OPTIONS, createLocalAccount: true, username: "Ana&João'O",
    removeCopilot: true, setComputerName: true, computerName: "PC-01",
  });
  assert.ok(xml.includes("<Name>Ana&amp;João&apos;O</Name>"));
  assert.ok(xml.includes("-Command &quot;"));
  assert.ok(xml.includes("&apos;Microsoft.Copilot&apos;"));
  assert.doesNotMatch(xml, /Ana&João|&amp;amp;/);
  // As regras NetBIOS rejeitam caracteres que precisariam de escape no ComputerName.
  for (const name of ["PC&01", "PC<01", 'PC"01', "PC'01"]) assert.notEqual(validateComputerName(name), null);
});

test("todas as 511 combinações ativas respeitam o limite e a estrutura básica do backend", () => {
  const keys = [
    "bypassWindows11Requirements", "createLocalAccount", "skipPrivacyQuestions",
    "disableEdgeBackground", "hideTaskbarSearch", "removeCopilot", "disableBitLocker",
    "bypassNetworkRequirement", "setComputerName",
  ] as const;
  for (let mask = 1; mask < 1 << keys.length; mask++) {
    const options = { ...NO_OPTIONS, username: "&".repeat(20), computerName: "A".repeat(15) };
    keys.forEach((key, index) => { options[key] = Boolean(mask & (1 << index)); });
    const xml = generateAutounattendXml(options);
    assert.ok(Buffer.byteLength(xml, "utf8") <= 16 * 1024, `combinação ${mask}`);
    assert.equal((xml.match(/<unattend\s/g) ?? []).length, 1);
    assert.equal((xml.match(/<\/unattend>/g) ?? []).length, 1);
    assert.match(xml, /<settings pass=/);
    assert.doesNotMatch(xml, /<!DOCTYPE|<!ENTITY|\0/i);
    for (const block of xml.matchAll(/<(?:RunSynchronous|FirstLogonCommands)>([\s\S]*?)<\/(?:RunSynchronous|FirstLogonCommands)>/g)) {
      const orders = [...block[1].matchAll(/<Order>(\d+)<\/Order>/g)].map((match) => Number(match[1]));
      assert.deepEqual(orders, orders.map((_, index) => index + 1));
    }
  }
});
