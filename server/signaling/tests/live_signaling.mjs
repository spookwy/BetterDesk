// Проверка сведения двух сторон через настоящий WebSocket.
// Node 22+ имеет встроенный WebSocket — npm-зависимостей не нужно.
const URL = 'ws://127.0.0.1:9100/ws';
const ID = '418207356';

const open = (name) => new Promise((res, rej) => {
  const ws = new WebSocket(URL);
  ws.onopen = () => res(ws);
  ws.onerror = (e) => rej(new Error(name + ': ' + e.message));
});

const wait = (ws, label) => new Promise((res, rej) => {
  const t = setTimeout(() => rej(new Error('таймаут ожидания: ' + label)), 4000);
  ws.addEventListener('message', (e) => { clearTimeout(t); res(e.data); }, { once: true });
});

let failures = 0;
const check = (cond, msg) => {
  console.log((cond ? 'OK  ' : 'FAIL') + '  ' + msg);
  if (!cond) failures++;
};

const host = await open('host');
const client = await open('client');

// 1. Регистрация хоста.
host.send(`register\t${ID}\t192.168.1.5:7000`);
const reg = await wait(host, 'registered');
check(reg.startsWith('registered\t' + ID), 'хост зарегистрирован: ' + reg);
check(reg.split('\t')[2].includes('127.0.0.1'), 'сервер вернул внешний адрес');

// 2. Хост слушает «к тебе стучатся» ДО того, как клиент постучится.
const wantsPromise = wait(host, 'wants');

// 3. Клиент ищет хоста.
client.send(`connect\t${ID}\t192.168.1.9:51000`);
const found = await wait(client, 'peer');
check(found.startsWith('peer\t' + ID), 'клиент получил адрес хоста: ' + found);

// 4. Хост узнал о клиенте — без этого NAT не пробить.
const wants = await wantsPromise;
check(wants.startsWith('wants\t' + ID), 'хост узнал о клиенте: ' + wants);

// Обе стороны за одним NAT (127.0.0.1) => отдаются ЛОКАЛЬНЫЕ адреса.
check(found.includes('192.168.1.5:7000'), 'в одной сети отдан локальный адрес хоста');
check(wants.includes('192.168.1.9:51000'), 'в одной сети отдан локальный адрес клиента');

// 5. Несуществующий ID — честная ошибка, а не зависание.
client.send(`connect\t111111111\t192.168.1.9:51000`);
const err = await wait(client, 'error');
check(err.startsWith('error\t'), 'на чужой ID пришла ошибка: ' + err);
check(err.includes('не в сети'), 'ошибка объясняет причину словами');

// 6. Мусор не роняет сервер и не рвёт соединение.
client.send('GET / HTTP/1.1');
const garbage = await wait(client, 'garbage');
check(garbage.startsWith('error\t'), 'мусор отвергнут с объяснением: ' + garbage);

// 7. Соединение живо после мусора.
client.send(`connect\t${ID}\t192.168.1.9:51000`);
const again = await wait(client, 'peer again');
check(again.startsWith('peer\t'), 'соединение пережило мусор');

// 8. Уход хоста убирает его из реестра.
host.close();
await new Promise(r => setTimeout(r, 400));
client.send(`connect\t${ID}\t192.168.1.9:51000`);
const gone = await wait(client, 'gone');
check(gone.startsWith('error\t'), 'ушедший хост больше не находится: ' + gone);

client.close();
console.log(failures === 0 ? '\nВСЁ ПРОЙДЕНО' : `\nПРОВАЛЕНО: ${failures}`);
process.exit(failures === 0 ? 0 : 1);
