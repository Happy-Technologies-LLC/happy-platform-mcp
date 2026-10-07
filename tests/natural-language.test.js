/**
 * Tests for the natural language parser (src/natural-language.js) and the
 * SN-Natural-Language-Search MCP tool that uses it.
 */

import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, jest, test } from '@jest/globals';
import http from 'node:http';
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { InMemoryTransport } from '@modelcontextprotocol/sdk/inMemory.js';
import * as naturalLanguage from '../src/natural-language.js';
import { createMcpServer } from '../src/mcp-server-consolidated.js';
import { ServiceNowClient } from '../src/servicenow-client.js';

const { parseNaturalLanguage, getSupportedPatterns, testParser } = naturalLanguage;

const MAX_QUERY_LENGTH = 2048;
const PROMPT_COMPLETION_MS = 50;

function parsed(query, table) {
  const { encodedQuery, unmatchedText } = parseNaturalLanguage(query, table);
  return { encodedQuery, unmatchedText };
}

/** Repeat `unit` after `prefix` and append `suffix`, without exceeding `length`. */
function filled(prefix, unit, suffix = '', length = MAX_QUERY_LENGTH) {
  const repeats = Math.floor((length - prefix.length - suffix.length) / unit.length);
  return `${prefix}${unit.repeat(repeats)}${suffix}`;
}

describe('parseNaturalLanguage input validation', () => {
  test('exports the 2048-character input limit', () => {
    expect(naturalLanguage.MAX_NATURAL_LANGUAGE_QUERY_LENGTH).toBe(MAX_QUERY_LENGTH);
  });

  test.each([
    ['undefined', undefined],
    ['null', null],
    ['number', 42],
    ['array', ['P1']],
    ['plain object', { query: 'P1' }],
    ['String object', new String('P1')]
  ])('rejects a %s query with a TypeError', (_label, query) => {
    expect(() => parseNaturalLanguage(query)).toThrow(TypeError);
    expect(() => parseNaturalLanguage(query)).toThrow('Natural language query must be a string');
  });

  test('rejects a non-string before invoking any of its coercion hooks', () => {
    const toString = jest.fn(() => 'P1');
    const valueOf = jest.fn(() => 'P1');

    expect(() => parseNaturalLanguage({ toString, valueOf })).toThrow(TypeError);
    expect(toString).not.toHaveBeenCalled();
    expect(valueOf).not.toHaveBeenCalled();
  });

  test('rejects a query one character over the limit without echoing it', () => {
    const query = `P1 ${'x'.repeat(MAX_QUERY_LENGTH - 2)}`;
    expect(query).toHaveLength(MAX_QUERY_LENGTH + 1);

    let error;
    try {
      parseNaturalLanguage(query);
    } catch (caught) {
      error = caught;
    }

    expect(error).toBeInstanceOf(RangeError);
    expect(error.message).toBe('Natural language query must be at most 2048 characters (received 2049)');
    expect(error.message).not.toContain('xxxx');
  });

  test('rejects an over-limit query that would otherwise be pathological', () => {
    const query = `with${' '.repeat(100_000)}!`;
    const startedAt = performance.now();

    expect(() => parseNaturalLanguage(query)).toThrow(RangeError);
    expect(performance.now() - startedAt).toBeLessThan(PROMPT_COMPLETION_MS);
  });

  test('accepts a query of exactly the maximum length', () => {
    const query = filled('P1 incidents about SAP', ' ', '');
    expect(query).toHaveLength(MAX_QUERY_LENGTH);

    expect(parsed(query)).toEqual({
      encodedQuery: 'priority=1^short_descriptionLIKESAP^ORdescriptionLIKESAP',
      unmatchedText: 'incidents'
    });
  });

  test('returns guidance for an empty query', () => {
    expect(parseNaturalLanguage('')).toEqual({
      encodedQuery: '',
      matchedPatterns: [],
      unmatchedText: '',
      suggestions: ['Please provide a valid query string']
    });
  });
});

describe('parseNaturalLanguage documented semantics', () => {
  // Examples from the SN-Natural-Language-Search tool description,
  // getSupportedPatterns(), testParser() and docs/NATURAL_LANGUAGE_SEARCH_IMPLEMENTATION.md.
  const documentedExamples = [
    ['find all P1 incidents', 'priority=1', 'find all incidents'],
    ['show recent problems assigned to me', 'assigned_to=javascript:gs.getUserID()^sys_created_on>javascript:gs.daysAgo(7)', 'show problems'],
    ['high priority changes created last week', 'priority=2', 'changes created last week'],
    ['open incidents about SAP', 'state=1^ORstate=2^ORstate=3^short_descriptionLIKESAP^ORdescriptionLIKESAP', 'incidents'],
    ['unassigned P2 incidents', 'assigned_toISEMPTY^priority=2', 'incidents'],
    ['unassigned P2 incidents created today', 'assigned_toISEMPTY^created_on>javascript:gs.daysAgoStart(0)^priority=2', 'incidents'],
    ['incidents created today with high priority', 'created_on>javascript:gs.daysAgoStart(0)^priority=2', 'incidents'],
    ['closed problems assigned to John Smith', 'assigned_to.nameLIKEJohn Smith^state=7', 'problems'],
    ['critical incidents opened in the last 30 days', 'sys_created_on>javascript:gs.daysAgo(30)', 'critical incidents'],
    ['show me my active tickets', 'active=true', 'show me my tickets'],
    ['new incidents with high impact and high urgency', 'impact=1^urgency=1^sys_created_on>javascript:gs.daysAgo(7)', 'incidents'],
    ['incidents containing database error', 'short_descriptionLIKEdatabase error^ORdescriptionLIKEdatabase error', 'incidents'],
    ['P1 incidents assigned to Network Team', 'assigned_to.nameLIKENetwork Team^priority=1', 'incidents'],
    ['incidents created after January 1', 'created_on>January 1', 'incidents'],
    ['resolved incidents about authentication', 'state=6^short_descriptionLIKEauthentication^ORdescriptionLIKEauthentication', 'incidents'],
    ['high priority incidents assigned to me', 'assigned_to=javascript:gs.getUserID()^priority=2', 'incidents'],
    ['recent problems about database', 'sys_created_on>javascript:gs.daysAgo(7)^short_descriptionLIKEdatabase^ORdescriptionLIKEdatabase', 'problems'],
    ['P1 incidents', 'priority=1', 'incidents'],
    ['recent problems', 'sys_created_on>javascript:gs.daysAgo(7)', 'problems'],
    ['high priority', 'priority=2', ''],
    ['P1', 'priority=1', ''],
    ['priority 2', 'priority=2', ''],
    ['critical priority', 'priority=1', ''],
    ['assigned to me', 'assigned_to=javascript:gs.getUserID()', ''],
    ['unassigned', 'assigned_toISEMPTY', ''],
    ['assigned to John Smith', 'assigned_to.nameLIKEJohn Smith', ''],
    ['my incidents', 'assigned_to=javascript:gs.getUserID()', ''],
    ['open', 'state=1^ORstate=2^ORstate=3', ''],
    ['active', 'active=true', ''],
    ['in progress', 'state=2', ''],
    ['closed', 'state=7', ''],
    ['resolved', 'state=6', ''],
    ['created today', 'created_on>javascript:gs.daysAgoStart(0)', ''],
    ['recent', 'sys_created_on>javascript:gs.daysAgo(7)', ''],
    ['opened yesterday', 'sys_created_on>javascript:gs.daysAgoStart(1)', ''],
    ['about SAP', 'short_descriptionLIKESAP^ORdescriptionLIKESAP', ''],
    ['containing error', 'short_descriptionLIKEerror^ORdescriptionLIKEerror', ''],
    ['description contains authentication', 'descriptionLIKEauthentication', ''],
    ['high impact', 'impact=1', ''],
    ['medium impact', 'impact=2', ''],
    ['low impact', 'impact=3', ''],
    ['high urgency', 'urgency=1', ''],
    ['medium urgency', 'urgency=2', ''],
    ['low urgency', 'urgency=3', ''],
    ['number is INC0012345', 'number=INC0012345', ''],
    ['caller is John Smith', 'caller_id.nameLIKEJohn Smith', ''],
    ['category is Software', 'categoryLIKESoftware', ''],
    ['assignment group is Network Team', 'assignment_group.nameLIKENetwork Team', ''],
    ['high priority and assigned to me', 'assigned_to=javascript:gs.getUserID()^priority=2', ''],
    ['recent and unassigned', 'assigned_toISEMPTY^sys_created_on>javascript:gs.daysAgo(7)', ''],
    ['P1 or P2 incidents', 'priority=1', 'P2 incidents']
  ];

  test.each(documentedExamples)('%j parses to %j', (query, encodedQuery, unmatchedText) => {
    expect(parsed(query)).toEqual({ encodedQuery, unmatchedText });
  });

  test('every getSupportedPatterns() and testParser() example is covered above', () => {
    const covered = new Set(documentedExamples.map(([query]) => query));
    const unparsable = new Set(['new', 'last 7 days', 'updated last week']);
    const examples = Object.values(getSupportedPatterns()).flatMap((entry) => entry.examples);

    for (const example of examples) {
      expect(covered.has(example) || unparsable.has(example)).toBe(true);
    }
    for (const { query } of testParser()) {
      expect(covered.has(query)).toBe(true);
    }
  });

  test('reports matched text and conditions in evaluation order', () => {
    expect(parseNaturalLanguage('unassigned P2 incidents created today')).toEqual({
      encodedQuery: 'assigned_toISEMPTY^created_on>javascript:gs.daysAgoStart(0)^priority=2',
      matchedPatterns: [
        { pattern: 'unassigned', matched: 'unassigned', condition: 'assigned_toISEMPTY' },
        { pattern: 'relative-day', matched: 'created today', condition: 'created_on>javascript:gs.daysAgoStart(0)' },
        { pattern: 'priority-code', matched: 'P2', condition: 'priority=2' }
      ],
      unmatchedText: 'incidents',
      suggestions: ['Unrecognized: "incidents"', 'Try using encoded query format: field=value^field2=value2', 'Supported patterns: priority (P1-P5), state (new/open/closed), assigned to me/unassigned, recent, dates']
    });
  });

  test('parses relative ranges, explicit dates and priority words', () => {
    expect(parsed('updated in the last 2 weeks')).toEqual({ encodedQuery: 'updated_on>javascript:gs.daysAgo(14)', unmatchedText: '' });
    expect(parsed('closed last 3 months')).toEqual({ encodedQuery: 'closed_on>javascript:gs.daysAgo(90)', unmatchedText: '' });
    expect(parsed('opened before March 5, 2024')).toEqual({ encodedQuery: 'sys_created_on<March 5, 2024', unmatchedText: '' });
    expect(parsed('created after June 30 2025 P3')).toEqual({ encodedQuery: 'created_on>June 30 2025^priority=3', unmatchedText: '' });
    expect(parsed('priority planning')).toEqual({ encodedQuery: 'priority=5', unmatchedText: '' });
    expect(parsed('PRIORITY 4 recently created')).toEqual({ encodedQuery: 'priority=4^sys_created_on>javascript:gs.daysAgo(7)', unmatchedText: '' });
    expect(parsed('number = CHG0000042')).toEqual({ encodedQuery: 'number=CHG0000042', unmatchedText: '' });
  });

  test('maps states per table and falls back to incident states', () => {
    expect(parsed('open', 'problem').encodedQuery).toBe('state=1^ORstate=2^ORstate=3^ORstate=4');
    expect(parsed('open', 'change_request').encodedQuery).toBe('state<0');
    expect(parsed('closed', 'change_request').encodedQuery).toBe('state=3');
    expect(parsed('on hold', 'cmdb_ci').encodedQuery).toBe('state=3');
    expect(parsed('closed', '__proto__').encodedQuery).toBe('state=7');
    expect(parsed('resolved', 'change_request').encodedQuery).toBe('state=resolved');
  });

  test('ends names and search terms only at whole connector words', () => {
    expect(parsed('assigned to John Andrews').encodedQuery).toBe('assigned_to.nameLIKEJohn Andrews');
    expect(parsed('caller is Ann Orr and P1').encodedQuery).toBe('priority=1^caller_id.nameLIKEAnn Orr');
    expect(parsed('about order incidents').encodedQuery).toBe('short_descriptionLIKEorder incidents^ORdescriptionLIKEorder incidents');
    expect(parsed('description contains "disk full" and P2').encodedQuery).toBe('priority=2^descriptionLIKEdisk full');
    expect(parsed("includes 'VPN' created today").encodedQuery).toBe('created_on>javascript:gs.daysAgoStart(0)^short_descriptionLIKEVPN^ORdescriptionLIKEVPN');
  });

  test('does not turn connectors or empty text into search terms', () => {
    expect(parsed('incidents with and').encodedQuery).toBe('');
    expect(parsed('category is or')).toEqual({ encodedQuery: '', unmatchedText: 'category is or' });
    expect(parsed("P1 about '").encodedQuery).toBe('priority=1');
  });

  test('cannot inject conditions or ordering through a description value', () => {
    const result = parseNaturalLanguage('description contains foo^active=false^ORDERBYnumber');

    // "active" is still read as the documented state word; the value with "^" is not captured.
    expect(result.encodedQuery).toBe('active=true');
    expect(result.matchedPatterns.map((match) => match.pattern)).toEqual(['state']);
    expect(result.encodedQuery).not.toContain('ORDERBY');
    expect(result.encodedQuery).not.toContain('active=false');
  });

  test('cannot inject ordering through a line break in a description value', () => {
    expect(parsed('description contains x\nORDERBYDESCsys_id').encodedQuery).toBe('');
    expect(parsed('description contains x\r\nORDERBYDESCsys_id and P1').encodedQuery).toBe('priority=1');
    expect(parsed('description contains disk full\n').encodedQuery).toBe('descriptionLIKEdisk full');
  });

  test('never captures "^", line breaks or control characters in any value', () => {
    const valuePrefixes = {
      'assigned-to-name': ['assigned_to.nameLIKE'],
      caller: ['caller_id.nameLIKE'],
      category: ['categoryLIKE'],
      'assignment-group': ['assignment_group.nameLIKE'],
      'description-contains': ['descriptionLIKE'],
      content: ['short_descriptionLIKE', '^ORdescriptionLIKE'],
      'before-date': ['<'],
      'after-date': ['>']
    };
    const capturedValues = (match) => {
      const separators = valuePrefixes[match.pattern];
      if (!separators) return [];
      if (match.pattern === 'content') {
        return match.condition.slice(separators[0].length).split(separators[1]);
      }
      return [match.condition.slice(match.condition.indexOf(separators[0]) + separators[0].length)];
    };
    const prefixes = [
      'assigned to John', 'caller is John', 'category = Software', 'assignment group is Network',
      'description contains foo', 'description contains "foo', 'about SAP', 'containing "SAP', 'with SAP',
      'includes SAP', 'created before January', 'opened after June 30,'
    ];
    const unsafe = ['^', '\n', '\r', '\r\n', '\t', '\u0000', '\u0007', '\u001b', '\u007f', '\u0085', '\u2028', '\u2029'];
    const tails = [' Smith', '1', ' 2024', 'ORDERBYnumber', ' active=false', '" and P1', ''];
    const forbidden = /[\^\u0000-\u001f\u007f-\u009f\u2028\u2029]/;
    let capturedCount = 0;

    for (const prefix of prefixes) {
      for (const character of unsafe) {
        for (const tail of tails) {
          for (const query of [`${prefix}${character}${tail}`, `${prefix} ${character} ${tail}`, `${prefix}${character}`]) {
            for (const match of parseNaturalLanguage(query).matchedPatterns) {
              for (const value of capturedValues(match)) {
                capturedCount++;
                expect({ query, value, unsafe: forbidden.test(value) }).toEqual({ query, value, unsafe: false });
              }
            }
          }
        }
      }
    }

    // The corpus must still exercise captures that legitimately end before the unsafe character.
    expect(capturedCount).toBeGreaterThan(100);
    expect(parsed('assigned to John\nand P1').encodedQuery).toBe('assigned_to.nameLIKEJohn^priority=1');
    expect(parsed('created before January 1\n').encodedQuery).toBe('created_on<January 1');
    expect(parsed('created before January\n1').encodedQuery).toBe('');
  });

  test('joins conditions with ^OR when unmatched text contains "or"', () => {
    expect(parsed('critical priority or unassigned')).toEqual({
      encodedQuery: 'assigned_toISEMPTY^ORpriority=1',
      unmatchedText: ''
    });
  });

  test('passes encoded queries through unchanged when no pattern matches', () => {
    expect(parseNaturalLanguage('category=software^impact=1')).toEqual({
      encodedQuery: 'category=software^impact=1',
      matchedPatterns: [],
      unmatchedText: '',
      suggestions: ['Using query as-is (appears to be encoded query format)']
    });
  });

  test('returns the original text when nothing matches', () => {
    const result = parseNaturalLanguage('show me everything');
    expect(result.encodedQuery).toBe('');
    expect(result.unmatchedText).toBe('show me everything');
    expect(result.suggestions[0]).toBe('No patterns matched. Supported patterns:');
  });
});

describe('parseNaturalLanguage bounded runtime', () => {
  const adversarialQueries = [
    ['content keyword followed by a whitespace run', `with${' '.repeat(MAX_QUERY_LENGTH - 5)}!`],
    ['assignee keyword followed by a whitespace run', `assigned to a${' '.repeat(MAX_QUERY_LENGTH - 14)}!`],
    ['repeated content keywords', filled('', 'with ')],
    ['repeated content keywords and spaces', filled('', `about${' '.repeat(15)}`, '!')],
    ['alternating and/or chain', filled('', 'and or ')],
    ['caller value made of connectors', filled('caller is ', 'and ', '1')],
    ['long assignee name ending in a digit', filled('assigned to ', 'a ', '1')],
    ['repeated field prefixes', filled('', 'assignment group is ')],
    ['unterminated quoted content', filled('about "', 'x ', '"!')],
    ['description followed by whitespace and a quote', filled('description contains ', ' ', 'x"')],
    ['only double quotes', '"'.repeat(MAX_QUERY_LENGTH)],
    ['only single quotes', "'".repeat(MAX_QUERY_LENGTH)],
    ['only spaces', ' '.repeat(MAX_QUERY_LENGTH)],
    ['mixed whitespace content keywords', filled('', 'with\t\n ')],
    ['repeated quoted content keywords', filled('', 'includes " ', '!')],
    ['repeated date prefixes', filled('', 'created before January ')],
    ['repeated relative ranges missing a unit', filled('', 'updated in the last 7 ')],
    ['repeated priority codes', filled('', 'p1 ')],
    ['one long word', filled('about ', 'a')],
    ['repeated connectors after content keyword', filled('about ', 'or ', '!')]
  ];

  beforeAll(() => {
    // Warm up the parser so the measurement reflects steady-state cost.
    parseNaturalLanguage('open incidents about SAP assigned to John Smith created today');
  });

  test.each(adversarialQueries)('%s completes promptly', (_label, query) => {
    expect(query.length).toBeLessThanOrEqual(MAX_QUERY_LENGTH);
    expect(query.length).toBeGreaterThan(MAX_QUERY_LENGTH - 25);

    const startedAt = performance.now();
    const result = parseNaturalLanguage(query);
    const elapsedMs = performance.now() - startedAt;

    expect(typeof result.encodedQuery).toBe('string');
    expect(elapsedMs).toBeLessThan(PROMPT_COMPLETION_MS);
  });

  test('still parses meaningful content inside a maximum-length adversarial query', () => {
    const query = filled('assigned to Jane Doe and P1 about', ' ', 'SAP');
    expect(query).toHaveLength(MAX_QUERY_LENGTH);

    const startedAt = performance.now();
    const result = parsed(query);

    expect(performance.now() - startedAt).toBeLessThan(PROMPT_COMPLETION_MS);
    expect(result).toEqual({
      encodedQuery: 'assigned_to.nameLIKEJane Doe^priority=1^short_descriptionLIKESAP^ORdescriptionLIKESAP',
      unmatchedText: ''
    });
  });
});

describe('SN-Natural-Language-Search through MCP', () => {
  let httpServer;
  let baseUrl;
  let requests;
  let mcpClient;
  let mcpServer;
  let consoleErrorSpy;

  beforeAll(async () => {
    httpServer = http.createServer((request, response) => {
      requests.push({ url: request.url, authorization: request.headers.authorization });
      response.writeHead(200, { 'Content-Type': 'application/json' });
      response.end(JSON.stringify({ result: [{ number: 'INC0010001', short_description: 'SAP outage' }] }));
    });
    await new Promise((resolve) => httpServer.listen(0, '127.0.0.1', resolve));
    baseUrl = `http://127.0.0.1:${httpServer.address().port}`;
  });

  afterAll(async () => {
    await new Promise((resolve) => httpServer.close(resolve));
  });

  beforeEach(async () => {
    requests = [];
    consoleErrorSpy = jest.spyOn(console, 'error').mockImplementation(() => {});

    const serviceNowClient = new ServiceNowClient(baseUrl, 'nl-smoke-user', 'nl-smoke-password');
    mcpServer = await createMcpServer(serviceNowClient);
    const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
    mcpClient = new Client({ name: 'natural-language-smoke', version: '1.0.0' });
    await Promise.all([mcpServer.connect(serverTransport), mcpClient.connect(clientTransport)]);
  });

  afterEach(async () => {
    await mcpClient.close();
    await mcpServer.close();
    consoleErrorSpy.mockRestore();
  });

  function textOf(result) {
    return result.content.map((part) => part.text).join('\n');
  }

  test('sends the parsed encoded query to the ServiceNow table API', async () => {
    const result = await mcpClient.callTool({
      name: 'SN-Natural-Language-Search',
      arguments: { query: 'open incidents about SAP', limit: 5 }
    });

    expect(result.isError).not.toBe(true);
    expect(requests).toHaveLength(1);
    const url = new URL(requests[0].url, baseUrl);
    expect(url.pathname).toBe('/api/now/table/incident');
    expect(url.searchParams.get('sysparm_query')).toBe('state=1^ORstate=2^ORstate=3^short_descriptionLIKESAP^ORdescriptionLIKESAP');
    expect(url.searchParams.get('sysparm_limit')).toBe('5');
    expect(requests[0].authorization).toBe(`Basic ${Buffer.from('nl-smoke-user:nl-smoke-password').toString('base64')}`);
    expect(textOf(result)).toContain('INC0010001');
  });

  test('answers a maximum-length adversarial query promptly without calling ServiceNow', async () => {
    const query = `with${' '.repeat(MAX_QUERY_LENGTH - 5)}!`;
    const startedAt = performance.now();
    const result = await mcpClient.callTool({
      name: 'SN-Natural-Language-Search',
      arguments: { query, show_patterns: false }
    });

    expect(performance.now() - startedAt).toBeLessThan(1_000);
    expect(result.isError).not.toBe(true);
    expect(textOf(result)).toContain('Unable to parse query');
    expect(requests).toHaveLength(0);
  });

  test('rejects an over-limit query as a tool error without calling ServiceNow', async () => {
    const result = await mcpClient.callTool({
      name: 'SN-Natural-Language-Search',
      arguments: { query: `P1 ${'x'.repeat(MAX_QUERY_LENGTH)}` }
    });

    expect(result.isError).toBe(true);
    expect(textOf(result)).toBe('Error: Natural language query must be at most 2048 characters (received 2051)');
    expect(requests).toHaveLength(0);
  });
});
