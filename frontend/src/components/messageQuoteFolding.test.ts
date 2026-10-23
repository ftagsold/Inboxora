import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import { plainTextBoundary, startsWithForwardMarker, startsWithReplyMarker } from './messageQuoteFolding.ts';

describe('message quote folding classification', () => {
  it('protects forwarded messages, including the Gmail delimiter', () => {
    assert.equal(startsWithForwardMarker('---------- Forwarded message ---------\nFrom: sender@example.com'), true);
    assert.equal(startsWithForwardMarker('Begin forwarded message:\nFrom: sender@example.com'), true);
    assert.equal(startsWithForwardMarker('New reply\n\n---------- Forwarded message ---------'), false);
  });

  it('recognizes common reply markers without treating forwards as replies', () => {
    assert.equal(startsWithReplyMarker('On Tue, Aug 25, 2026 Name <n@example.com> wrote:\nOld body'), true);
    assert.equal(startsWithReplyMarker('Dnia 25 sierpnia 2026 Jan napisał(a):\nStara treść'), true);
    assert.equal(startsWithReplyMarker('-----Original Message-----\nFrom: sender@example.com'), true);
    assert.equal(startsWithReplyMarker('---------- Forwarded message ---------'), false);
  });

  it('folds a plain-text reply tail but keeps a forwarded message fully visible', () => {
    const reply = 'Current answer\n\nOn Tue, Name wrote:\n> old line one\n> old line two';
    assert.equal(plainTextBoundary(reply), reply.indexOf('On Tue'));
    const quotedTail = 'Current answer\n\n> old line one\n> old line two';
    assert.equal(plainTextBoundary(quotedTail), quotedTail.indexOf('> old'));
    assert.equal(plainTextBoundary('FYI\n\n---------- Forwarded message ---------\nFrom: sender@example.com'), -1);
    const nestedForward = 'FYI\n\n---------- Forwarded message ---------\nFrom: sender@example.com\n\nImmediate forwarded body\n\nDnia 25 sierpnia 2026 Jan napisał(a):\n> older reply';
    assert.equal(plainTextBoundary(nestedForward), nestedForward.indexOf('Dnia 25'));
  });

  it('does not fold a single incidental greater-than line or interleaved replies', () => {
    assert.equal(plainTextBoundary('Note\n> one quoted-looking line'), -1);
    assert.equal(plainTextBoundary('> question one\nanswer one\n> question two\nanswer two'), -1);
  });
});

describe('reply formats beyond Gmail', () => {
  it('recognises German, Outlook header-block and wrapped reply markers', () => {
    assert.equal(startsWithReplyMarker('Am 21.10.2026 um 10:00 schrieb Max Mustermann <max@example.test>:'), true);
    assert.equal(startsWithReplyMarker('Von: Max Mustermann <max@example.test>\nGesendet: Dienstag, 21. Oktober 2026\nAn: me@example.test\nBetreff: Planung'), true);
    assert.equal(startsWithReplyMarker('From: Max <max@example.test>\nSent: Tuesday, October 21, 2026 10:00\nTo: me@example.test'), true);
    assert.equal(startsWithReplyMarker('-----Ursprüngliche Nachricht-----\nVon: Max'), true);
    assert.equal(startsWithReplyMarker('On Tue, Oct 21, 2026 at 10:00 AM\nMax Mustermann <max@example.test> wrote:'), true);
  });

  it('does not treat ordinary text as a reply header', () => {
    assert.equal(startsWithReplyMarker('From: the team, thanks for all the work\nSee you on Monday'), false);
    assert.equal(startsWithReplyMarker('Am Montag treffen wir uns.\nBis dann'), false);
  });

  it('cuts plain-text bodies at a German or Outlook-style reply header', () => {
    const german = 'Antwort\n\nAm 21.10.2026 schrieb Max:\n> alt';
    assert.equal(plainTextBoundary(german), german.indexOf('Am 21.10.2026'));
    const outlook = 'Answer\n\nFrom: Max <max@example.test>\nSent: Tuesday\nTo: me\n\nOld text';
    assert.equal(plainTextBoundary(outlook), outlook.indexOf('From: Max'));
  });
});

describe('HTML header blocks flattened by textContent', () => {
  it('recognises a From/Sent header whose lines were separated by <br>', () => {
    assert.equal(startsWithReplyMarker('Von: Max Mustermann <max@example.test>Gesendet: Dienstag, 21. Oktober 2026 10:00An: me@example.test'), true);
    assert.equal(startsWithReplyMarker('From: Max <max@example.test> Sent: Tuesday, October 21, 2026 To: me'), true);
  });
});

describe('inline header label boundaries', () => {
  it('ignores a label that is only the end of another word', () => {
    assert.equal(startsWithReplyMarker('From: the release notes Update: new version is out'), false);
  });
});

describe('dated sender reply markers', () => {
  it('recognises "date time Name <address>:" headers', () => {
    assert.equal(startsWithReplyMarker('16.10.2026 19:57:53 Maria Tagsold <maria@tagsold.io>:'), true);
    assert.equal(startsWithReplyMarker('16.10.2026 16:35:45 Reservierung | Castlewood Hotels GmbH <Reservierung@castlewood-hotels.com>:\nSehr geehrte Frau Tagsold,'), true);
    assert.equal(startsWithReplyMarker('2026-10-16, 19:57 Maria <maria@example.test>:'), true);
  });

  it('leaves ordinary dated lines alone', () => {
    assert.equal(startsWithReplyMarker('16.10.2026 19:57 Termin bestätigt'), false);
    assert.equal(startsWithReplyMarker('Wäre es denn vom 13. auf den 14.11. möglich?'), false);
  });
});

describe('attribution lines across clients and languages', () => {
  it('recognises translated "wrote:" attributions', () => {
    assert.equal(startsWithReplyMarker('Op 21 okt. 2026 om 10:00 schreef Max <max@example.test>:'), true);
    assert.equal(startsWithReplyMarker('Den 21 okt. 2026 kl. 10:00 skrev Max <max@example.test>:'), true);
    assert.equal(startsWithReplyMarker('Em ter., 21 de out. de 2026 às 10:00, Max <max@example.test> escreveu:'), true);
    assert.equal(startsWithReplyMarker('W dniu 21.10.2026 o 10:00, Jan Kowalski pisze:\n> stare') || startsWithReplyMarker('W dniu 21.10.2026 o 10:00 Jan napisał:'), true);
    assert.equal(startsWithReplyMarker('21.10.2026, 10:00, "Иван" <ivan@example.test> написал(а):'), true);
    assert.equal(startsWithReplyMarker('2026年10月21日(火) 10:00 Max <max@example.test>:'), true);
    assert.equal(startsWithReplyMarker('Max <max@example.test> 于2026年10月21日周二 10:00写道：'), true);
  });

  it('recognises Android and Outlook separators', () => {
    assert.equal(startsWithReplyMarker('-------- Original message --------\nFrom: Max'), true);
    assert.equal(startsWithReplyMarker('---- Max wrote ----'), true);
    assert.equal(startsWithReplyMarker('________________________________\nFrom: Max <max@example.test>\nSent: Tuesday\nTo: me'), true);
    assert.equal(startsWithReplyMarker('Van: Max <max@example.test>\nVerzonden: dinsdag 21 oktober 2026\nAan: me'), true);
  });

  it('keeps prose that merely ends in "wrote:" visible', () => {
    assert.equal(startsWithReplyMarker('Here is what the author wrote:'), false);
  });

  it('includes the rule line above an Outlook header in the plain-text fold', () => {
    const outlook = 'Answer\n\n________________________________\nFrom: Max <max@example.test>\nSent: Tuesday\n\nOld';
    assert.equal(plainTextBoundary(outlook), outlook.indexOf('____'));
  });
});
