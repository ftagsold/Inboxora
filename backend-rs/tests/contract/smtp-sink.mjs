// A minimal SMTP server for contract scenarios: accepts any AUTH PLAIN/LOGIN, records every
// message (envelope + raw data) and never offers STARTTLS.
import net from 'node:net';

const sinks = new Map();

export function sink(port) {
  if (sinks.has(port)) return sinks.get(port);
  const state = { port, messages: [], auths: [] };
  const server = net.createServer((socket) => {
    let buffer = '';
    let mode = 'command';
    let current = { from: null, to: [], data: '' };
    const reply = (line) => socket.write(`${line}\r\n`);
    reply('220 sink.test ESMTP ready');
    socket.on('data', (chunk) => {
      buffer += chunk.toString('utf8');
      let idx;
      while ((idx = buffer.indexOf('\r\n')) >= 0) {
        const line = buffer.slice(0, idx);
        buffer = buffer.slice(idx + 2);
        if (mode === 'data') {
          if (line === '.') {
            state.messages.push({ ...current, data: current.data });
            current = { from: null, to: [], data: '' };
            mode = 'command';
            reply('250 2.0.0 queued');
          } else {
            current.data += `${line.startsWith('..') ? line.slice(1) : line}\r\n`;
          }
          continue;
        }
        if (mode === 'auth-login-user') { state.auths.push(Buffer.from(line, 'base64').toString()); mode = 'auth-login-pass'; reply('334 UGFzc3dvcmQ6'); continue; }
        if (mode === 'auth-login-pass') { mode = 'command'; reply('235 2.7.0 accepted'); continue; }
        const [verb, ...rest] = line.split(' ');
        switch (verb.toUpperCase()) {
          case 'EHLO': socket.write('250-sink.test\r\n250-AUTH PLAIN LOGIN\r\n250-8BITMIME\r\n250 SMTPUTF8\r\n'); break;
          case 'HELO': reply('250 sink.test'); break;
          case 'AUTH':
            if ((rest[0] || '').toUpperCase() === 'PLAIN') { state.auths.push(Buffer.from(rest[1] || '', 'base64').toString().split('\0')[1]); reply('235 2.7.0 accepted'); }
            else { mode = 'auth-login-user'; reply('334 VXNlcm5hbWU6'); }
            break;
          case 'MAIL': current.from = line.match(/<([^>]*)>/)?.[1] ?? null; reply('250 2.1.0 ok'); break;
          case 'RCPT': current.to.push(line.match(/<([^>]*)>/)?.[1]); reply('250 2.1.5 ok'); break;
          case 'DATA': mode = 'data'; reply('354 go ahead'); break;
          case 'RSET': current = { from: null, to: [], data: '' }; reply('250 ok'); break;
          case 'NOOP': reply('250 ok'); break;
          case 'QUIT': reply('221 bye'); socket.end(); break;
          default: reply('502 unsupported');
        }
      }
    });
    socket.on('error', () => {});
  });
  server.listen(port, '127.0.0.1');
  server.unref();
  state.server = server;
  // The decoded text of the latest message (quoted-printable soft breaks and =XX undone).
  // Base64 runs (lines of base64 alphabet only) are decoded and appended, so either transfer
  // encoding the mailer picks can be searched.
  state.lastText = () => {
    const raw = state.messages.at(-1)?.data || '';
    const qp = raw.replace(/=\r\n/g, '').replace(/=([0-9A-F]{2})/g, (_, h) => String.fromCharCode(parseInt(h, 16)));
    const runs = raw.split(/\r\n\r\n/).map(block => block.split('\r\n').filter(l => /^[A-Za-z0-9+/]+=*$/.test(l) && l.length >= 16).join(''))
      .filter(Boolean).map(b => Buffer.from(b, 'base64').toString('utf8'));
    return [qp, ...runs].join('\n');
  };
  sinks.set(port, state);
  return state;
}
