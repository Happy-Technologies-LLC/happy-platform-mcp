/**
 * Happy MCP Server - Natural Language Query Processing
 *
 * Copyright (c) 2025 Happy Technologies LLC
 * Licensed under the MIT License - see LICENSE file for details
 *
 * Natural Language to ServiceNow Encoded Query Parser
 * Converts human-readable queries into ServiceNow encoded query strings.
 *
 * The input is validated (string, at most MAX_NATURAL_LANGUAGE_QUERY_LENGTH
 * characters) before any parsing. It is then tokenized in a single linear
 * pass, and every supported pattern is matched over that token list with
 * fixed lookahead or a single forward scan. No pattern uses a backtracking
 * regular expression, so parsing time grows linearly with the bounded input.
 *
 * @module natural-language
 */

/** Maximum accepted natural language query length, in UTF-16 code units. */
export const MAX_NATURAL_LANGUAGE_QUERY_LENGTH = 2048;

/**
 * Table-specific state conditions
 */
const STATE_MAPPINGS = {
  incident: {
    'new': 'state=1',
    'in progress': 'state=2',
    'on hold': 'state=3',
    'resolved': 'state=6',
    'closed': 'state=7',
    'canceled': 'state=8',
    'open': 'state=1^ORstate=2^ORstate=3', // New, In Progress, or On Hold
    'active': 'active=true'
  },
  change_request: {
    'new': 'state=-5',
    'assess': 'state=-4',
    'authorize': 'state=-3',
    'scheduled': 'state=-2',
    'implement': 'state=-1',
    'review': 'state=0',
    'closed': 'state=3',
    'canceled': 'state=4',
    'open': 'state<0', // Negative states are open
    'active': 'active=true'
  },
  problem: {
    'new': 'state=1',
    'assessed': 'state=2',
    'root cause analysis': 'state=3',
    'fix in progress': 'state=4',
    'resolved': 'state=6',
    'closed': 'state=7',
    'open': 'state=1^ORstate=2^ORstate=3^ORstate=4',
    'active': 'active=true'
  }
};

/**
 * Priority mappings (consistent across tables)
 */
const PRIORITY_MAPPINGS = {
  'critical': '1',
  'high': '2',
  'moderate': '3',
  'low': '4',
  'planning': '5',
  '1': '1',
  '2': '2',
  '3': '3',
  '4': '4',
  '5': '5'
};

/**
 * Impact and urgency mappings
 */
const IMPACT_MAPPINGS = {
  'high': '1',
  'medium': '2',
  'low': '3'
};

const URGENCY_MAPPINGS = {
  'high': '1',
  'medium': '2',
  'low': '3'
};

const WORD = 'word';
const SPACE = 'space';
const SYMBOL = 'symbol';

const WHITESPACE_CHARACTER = /\s/;
const PRIORITY_CODE = /^[Pp][1-5]$/;
const ALPHA_WORD = /^[A-Za-z]+$/;
const ALPHANUMERIC_WORD = /^[A-Za-z0-9]+$/;
const DIGITS = /^[0-9]+$/;
const DAY_OF_MONTH = /^[0-9]{1,2}$/;
const YEAR = /^[0-9]{4}$/;
const RECORD_NUMBER = /^[A-Za-z]{3}[0-9]{7}$/;

const DATE_FIELDS = new Set(['created', 'opened', 'updated', 'modified', 'closed']);
const PRIORITY_LEVELS = new Set(['critical', 'high', 'moderate', 'low', 'planning']);
const SEVERITY_LEVELS = new Set(['high', 'medium', 'low']);
const MY_RECORD_WORDS = new Set(['incidents', 'problems', 'changes', 'tickets']);
const RELATIVE_DAYS = new Set(['today', 'yesterday']);
const CONNECTORS = new Set(['and', 'or']);
const RANGE_UNITS = new Set(['day', 'days', 'week', 'weeks', 'month', 'months']);
const SINGLE_WORD_STATES = new Set(['new', 'open', 'active', 'resolved', 'closed', 'canceled']);
const CONTENT_KEYWORDS = new Set(['about', 'containing', 'with', 'include', 'includes']);
const DESCRIPTION_VERBS = new Set(['contains', 'includes']);
const NUMBER_OPERATORS = new Set(['is', 'equal', 'equals']);
const FILLER_WORDS = new Set(['and', 'or', 'with', 'in', 'the', 'a', 'an']);

const ASSIGNEE_TERMINATORS = new Set(['and', 'or', 'with', 'created', 'opened', 'updated']);
const FIELD_VALUE_TERMINATORS = new Set(['and', 'or', 'with']);
const CONTENT_TERMINATORS = new Set(['and', 'or', 'in', 'with', 'created', 'opened', 'assigned']);
const DESCRIPTION_TERMINATORS = new Set(['and', 'or']);

function isWordCharacter(code) {
  return (code >= 48 && code <= 57) // 0-9
    || (code >= 65 && code <= 90) // A-Z
    || (code >= 97 && code <= 122) // a-z
    || code === 95; // _
}

/**
 * Characters that may never appear in a captured value: the encoded query
 * separator "^", C0/C1 control characters (including CR, LF and tab) and the
 * Unicode line/paragraph separators.
 */
function isUnsafeValueCharacter(code) {
  return code === 94 // ^
    || code < 32
    || (code >= 127 && code <= 159)
    || code === 0x2028
    || code === 0x2029;
}

/**
 * Split the query into maximal word runs ([A-Za-z0-9_], matching regex \w),
 * maximal whitespace runs, and single other characters. One linear pass.
 * Whitespace and symbol tokens containing an unsafe value character are
 * flagged `unsafe` so no pattern can copy them into an encoded query value.
 */
function tokenize(query) {
  const tokens = [];
  let index = 0;

  while (index < query.length) {
    const start = index;
    if (isWordCharacter(query.charCodeAt(index))) {
      while (index < query.length && isWordCharacter(query.charCodeAt(index))) index++;
      const text = query.slice(start, index);
      tokens.push({ type: WORD, text, lower: text.toLowerCase() });
    } else if (WHITESPACE_CHARACTER.test(query[index])) {
      let unsafe = false;
      while (index < query.length && WHITESPACE_CHARACTER.test(query[index])) {
        unsafe ||= isUnsafeValueCharacter(query.charCodeAt(index));
        index++;
      }
      tokens.push({ type: SPACE, text: query.slice(start, index), unsafe });
    } else {
      index++;
      tokens.push({ type: SYMBOL, text: query[start], unsafe: isUnsafeValueCharacter(query.charCodeAt(start)) });
    }
  }

  return tokens;
}

function isWord(token, word) {
  return token !== undefined && token.type === WORD && token.lower === word;
}

function isWordIn(token, words) {
  return token !== undefined && token.type === WORD && words.has(token.lower);
}

function isWordLike(token, pattern) {
  return token !== undefined && token.type === WORD && pattern.test(token.text);
}

function isSpace(token) {
  return token !== undefined && token.type === SPACE;
}

/** Whitespace that may be copied into a captured value. */
function isValueSpace(token) {
  return isSpace(token) && !token.unsafe;
}

function isSymbol(token, symbol) {
  return token !== undefined && token.type === SYMBOL && token.text === symbol;
}

function isQuote(token) {
  return isSymbol(token, '"') || isSymbol(token, "'");
}

function textOf(tokens, start, end) {
  let text = '';
  for (let index = start; index < end; index++) {
    text += tokens[index].text;
  }
  return text;
}

function dateField(token) {
  return token.lower === 'opened' ? 'sys_created_on' : `${token.lower}_on`;
}

/**
 * Return the leftmost match produced by `matchAt` (which inspects a fixed
 * number of tokens starting at an index), or null.
 */
function findFirst(tokens, matchAt) {
  for (let index = 0; index < tokens.length; index++) {
    const match = matchAt(index);
    if (match) {
      return match;
    }
  }
  return null;
}

/**
 * Leftmost "<prefix> <value>" match where the value is a lazily captured run
 * of accepted tokens ending at the end of the query, or before whitespace and
 * a terminator word (consumed with the match). `capturePrefix(index)` returns
 * the index of the first value token when a prefix starts at `index`. A value
 * must contain a word, cannot start with a logical connector ("and"/"or") and
 * never includes an `unsafe` token ("^", control characters, line breaks).
 *
 * A failed value scan is never repeated for later prefixes whose value starts
 * inside the already rejected span: they would traverse the same tokens and
 * fail at the same token. Total work is therefore linear in the token count.
 */
function findCapture(tokens, { capturePrefix, accepts, terminators, quoted }) {
  let rejectedThrough = -1;

  for (let index = 0; index < tokens.length; index++) {
    const valueStart = capturePrefix(index);
    if (valueStart < 0 || valueStart <= rejectedThrough) {
      continue;
    }

    let hasWord = false;
    for (let position = valueStart; ; position++) {
      const token = tokens[position];
      if (token === undefined || token.unsafe || !accepts(token)) {
        rejectedThrough = position;
        break;
      }
      if (token.type === WORD) {
        if (!hasWord && CONNECTORS.has(token.lower)) {
          rejectedThrough = position;
          break;
        }
        hasWord = true;
      }
      if (!hasWord) {
        continue;
      }

      let next = position + 1;
      if (quoted && isQuote(tokens[next])) {
        next++;
      }
      let end = -1;
      if (next === tokens.length) {
        end = next;
      } else if (isSpace(tokens[next])) {
        if (next + 1 === tokens.length) {
          end = next + 1;
        } else if (isWordIn(tokens[next + 1], terminators)) {
          end = next + 2;
        }
      }
      if (end >= 0) {
        return { start: index, end, value: textOf(tokens, valueStart, position + 1).trim() };
      }
    }
  }

  return null;
}

/**
 * "<keyword...> (is|=) <value>" prefix for field value patterns.
 */
function fieldValuePrefix(tokens, keywords) {
  return (index) => {
    let position = index;
    for (const keyword of keywords) {
      if (position > index && !isSpace(tokens[position++])) return -1;
      if (!isWord(tokens[position++], keyword)) return -1;
    }
    if (!isSpace(tokens[position++])) return -1;
    const operator = tokens[position++];
    if (!isWord(operator, 'is') && !isSymbol(operator, '=')) return -1;
    if (!isSpace(tokens[position++])) return -1;
    return position;
  };
}

const acceptsName = (token) => token.type === SPACE || (token.type === WORD && ALPHA_WORD.test(token.text));
const acceptsContent = (token) => token.type === SPACE || (token.type === WORD && ALPHANUMERIC_WORD.test(token.text));
const acceptsUnquoted = (token) => !isQuote(token);

function fieldValuePattern(id, keywords, fieldName) {
  return {
    id,
    find: (tokens) => {
      const match = findCapture(tokens, {
        capturePrefix: fieldValuePrefix(tokens, keywords),
        accepts: acceptsName,
        terminators: FIELD_VALUE_TERMINATORS
      });
      return match && { ...match, condition: `${fieldName}LIKE${match.value}` };
    }
  };
}

function explicitDatePattern(id, keyword, operator) {
  return {
    id,
    find: (tokens) => findFirst(tokens, (index) => {
      const field = tokens[index];
      if (!isWordIn(field, DATE_FIELDS) || !isSpace(tokens[index + 1])
        || !isWord(tokens[index + 2], keyword) || !isSpace(tokens[index + 3])
        || !isWordLike(tokens[index + 4], ALPHA_WORD) || !isValueSpace(tokens[index + 5])
        || !isWordLike(tokens[index + 6], DAY_OF_MONTH)) {
        return null;
      }

      let end = index + 7;
      let yearStart = end;
      if (isSymbol(tokens[yearStart], ',')) yearStart++;
      if (isValueSpace(tokens[yearStart]) && isWordLike(tokens[yearStart + 1], YEAR)) {
        end = yearStart + 2;
      }

      return {
        start: index,
        end,
        condition: `${dateField(field)}${operator}${textOf(tokens, index + 4, end)}`
      };
    })
  };
}

/**
 * Supported patterns in evaluation order (highest precedence first). Each
 * pattern is applied at most once, to its leftmost match in the text left
 * unmatched by earlier patterns.
 */
const PATTERNS = [
  // Number patterns
  {
    id: 'number',
    find: (tokens) => findFirst(tokens, (index) => {
      const operator = tokens[index + 2];
      if (!isWord(tokens[index], 'number') || !isSpace(tokens[index + 1])
        || !(isWordIn(operator, NUMBER_OPERATORS) || isSymbol(operator, '='))
        || !isSpace(tokens[index + 3]) || !isWordLike(tokens[index + 4], RECORD_NUMBER)) {
        return null;
      }
      return { start: index, end: index + 5, condition: `number=${tokens[index + 4].text}` };
    })
  },

  // Assignment patterns
  {
    id: 'assigned-to-me',
    find: (tokens) => findFirst(tokens, (index) => {
      if (isWord(tokens[index], 'assigned') && isSpace(tokens[index + 1])
        && isWord(tokens[index + 2], 'to') && isSpace(tokens[index + 3])
        && isWord(tokens[index + 4], 'me')) {
        return { start: index, end: index + 5, condition: 'assigned_to=javascript:gs.getUserID()' };
      }
      if (isWord(tokens[index], 'my') && isSpace(tokens[index + 1])
        && isWordIn(tokens[index + 2], MY_RECORD_WORDS)) {
        return { start: index, end: index + 3, condition: 'assigned_to=javascript:gs.getUserID()' };
      }
      return null;
    })
  },
  {
    id: 'unassigned',
    find: (tokens) => findFirst(tokens, (index) => (
      isWord(tokens[index], 'unassigned')
        ? { start: index, end: index + 1, condition: 'assigned_toISEMPTY' }
        : null
    ))
  },
  {
    id: 'assigned-to-name',
    find: (tokens) => {
      const match = findCapture(tokens, {
        capturePrefix: (index) => (
          isWord(tokens[index], 'assigned') && isSpace(tokens[index + 1])
            && isWord(tokens[index + 2], 'to') && isSpace(tokens[index + 3])
            ? index + 4
            : -1
        ),
        accepts: acceptsName,
        terminators: ASSIGNEE_TERMINATORS
      });
      // Note: This creates a LIKE query - could be enhanced with user lookup
      return match && { ...match, condition: `assigned_to.nameLIKE${match.value}` };
    }
  },

  // Date patterns - relative
  {
    id: 'relative-day',
    find: (tokens) => findFirst(tokens, (index) => {
      const field = tokens[index];
      const day = tokens[index + 2];
      if (!isWordIn(field, DATE_FIELDS) || !isSpace(tokens[index + 1]) || !isWordIn(day, RELATIVE_DAYS)) {
        return null;
      }
      const days = day.lower === 'today' ? 0 : 1;
      return { start: index, end: index + 3, condition: `${dateField(field)}>javascript:gs.daysAgoStart(${days})` };
    })
  },
  {
    id: 'relative-range',
    find: (tokens) => findFirst(tokens, (index) => {
      const field = tokens[index];
      if (!isWordIn(field, DATE_FIELDS) || !isSpace(tokens[index + 1])) {
        return null;
      }
      let position = index + 2;
      if (isWord(tokens[position], 'in') && isSpace(tokens[position + 1])) position += 2;
      if (isWord(tokens[position], 'the') && isSpace(tokens[position + 1])) position += 2;
      const amount = tokens[position + 2];
      const unit = tokens[position + 4];
      if (!isWord(tokens[position], 'last') || !isSpace(tokens[position + 1])
        || !isWordLike(amount, DIGITS) || !isSpace(tokens[position + 3])
        || !isWordIn(unit, RANGE_UNITS)) {
        return null;
      }

      let days = parseInt(amount.text, 10);
      if (unit.lower.startsWith('week')) days *= 7;
      if (unit.lower.startsWith('month')) days *= 30;

      return { start: index, end: position + 5, condition: `${dateField(field)}>javascript:gs.daysAgo(${days})` };
    })
  },
  explicitDatePattern('before-date', 'before', '<'),
  explicitDatePattern('after-date', 'after', '>'),

  // Priority patterns
  {
    id: 'priority-level',
    find: (tokens) => findFirst(tokens, (index) => {
      const level = tokens[index];
      if (!isWordIn(level, PRIORITY_LEVELS) || !isSpace(tokens[index + 1]) || !isWord(tokens[index + 2], 'priority')) {
        return null;
      }
      return { start: index, end: index + 3, condition: `priority=${PRIORITY_MAPPINGS[level.lower]}` };
    })
  },
  {
    id: 'priority-code',
    find: (tokens) => findFirst(tokens, (index) => (
      isWordLike(tokens[index], PRIORITY_CODE)
        ? { start: index, end: index + 1, condition: `priority=${tokens[index].text[1]}` }
        : null
    ))
  },
  {
    id: 'priority-value',
    find: (tokens) => findFirst(tokens, (index) => {
      const level = tokens[index + 2];
      if (!isWord(tokens[index], 'priority') || !isSpace(tokens[index + 1])
        || level === undefined || level.type !== WORD || !Object.hasOwn(PRIORITY_MAPPINGS, level.lower)) {
        return null;
      }
      return { start: index, end: index + 3, condition: `priority=${PRIORITY_MAPPINGS[level.lower]}` };
    })
  },

  // Caller, category and assignment group patterns
  fieldValuePattern('caller', ['caller'], 'caller_id.name'),
  fieldValuePattern('category', ['category'], 'category'),
  fieldValuePattern('assignment-group', ['assignment', 'group'], 'assignment_group.name'),

  // Impact and urgency patterns
  {
    id: 'impact',
    find: (tokens) => findFirst(tokens, (index) => {
      const level = tokens[index];
      if (!isWordIn(level, SEVERITY_LEVELS) || !isSpace(tokens[index + 1]) || !isWord(tokens[index + 2], 'impact')) {
        return null;
      }
      return { start: index, end: index + 3, condition: `impact=${IMPACT_MAPPINGS[level.lower]}` };
    })
  },
  {
    id: 'urgency',
    find: (tokens) => findFirst(tokens, (index) => {
      const level = tokens[index];
      if (!isWordIn(level, SEVERITY_LEVELS) || !isSpace(tokens[index + 1]) || !isWord(tokens[index + 2], 'urgency')) {
        return null;
      }
      return { start: index, end: index + 3, condition: `urgency=${URGENCY_MAPPINGS[level.lower]}` };
    })
  },

  // Recent records
  {
    id: 'recent',
    find: (tokens) => findFirst(tokens, (index) => {
      const token = tokens[index];
      if (isWord(token, 'recent') || isWord(token, 'new')) {
        return { start: index, end: index + 1, condition: 'sys_created_on>javascript:gs.daysAgo(7)' };
      }
      if (isWord(token, 'recently') && isSpace(tokens[index + 1]) && isWord(tokens[index + 2], 'created')) {
        return { start: index, end: index + 3, condition: 'sys_created_on>javascript:gs.daysAgo(7)' };
      }
      return null;
    })
  },

  // State patterns (table-dependent)
  {
    id: 'state',
    find: (tokens, table) => findFirst(tokens, (index) => {
      const token = tokens[index];
      let state = null;
      let end = index + 1;
      if (isWordIn(token, SINGLE_WORD_STATES)) {
        state = token.lower;
      } else if (isWord(token, 'in') && isSpace(tokens[index + 1]) && isWord(tokens[index + 2], 'progress')) {
        state = 'in progress';
        end = index + 3;
      } else if (isWord(token, 'on') && isSpace(tokens[index + 1]) && isWord(tokens[index + 2], 'hold')) {
        state = 'on hold';
        end = index + 3;
      } else {
        return null;
      }

      const mapping = Object.hasOwn(STATE_MAPPINGS, table) ? STATE_MAPPINGS[table] : STATE_MAPPINGS.incident;
      const condition = Object.hasOwn(mapping, state) ? mapping[state] : `state=${state}`;
      return { start: index, end, condition };
    })
  },

  // Content search patterns
  {
    id: 'description-contains',
    find: (tokens) => {
      const match = findCapture(tokens, {
        capturePrefix: (index) => {
          if (!isWord(tokens[index], 'description') || !isSpace(tokens[index + 1])
            || !isWordIn(tokens[index + 2], DESCRIPTION_VERBS) || !isSpace(tokens[index + 3])) {
            return -1;
          }
          return isQuote(tokens[index + 4]) ? index + 5 : index + 4;
        },
        accepts: acceptsUnquoted,
        terminators: DESCRIPTION_TERMINATORS,
        quoted: true
      });
      return match && { ...match, condition: `descriptionLIKE${match.value}` };
    }
  },
  {
    id: 'content',
    find: (tokens) => {
      const match = findCapture(tokens, {
        capturePrefix: (index) => {
          if (!isWordIn(tokens[index], CONTENT_KEYWORDS) || !isSpace(tokens[index + 1])) {
            return -1;
          }
          return isQuote(tokens[index + 2]) ? index + 3 : index + 2;
        },
        accepts: acceptsContent,
        terminators: CONTENT_TERMINATORS,
        quoted: true
      });
      return match && {
        ...match,
        condition: `short_descriptionLIKE${match.value}^ORdescriptionLIKE${match.value}`
      };
    }
  }
];

/**
 * Replace tokens[start, end) with one space, merge it into neighbouring
 * whitespace and trim the token list ends.
 */
function removeMatch(tokens, start, end) {
  let from = start;
  let to = end;
  if (isSpace(tokens[from - 1])) from--;
  if (isSpace(tokens[to])) to++;
  tokens.splice(from, to - from, { type: SPACE, text: ' ' });

  if (isSpace(tokens[tokens.length - 1])) tokens.pop();
  if (isSpace(tokens[0])) tokens.shift();
}

/**
 * Remaining text with filler words removed and whitespace collapsed.
 */
function describeUnmatched(tokens) {
  let text = '';
  let separated = false;
  for (const token of tokens) {
    if (token.type === SPACE || isWordIn(token, FILLER_WORDS)) {
      separated = true;
      continue;
    }
    text += separated && text.length > 0 ? ` ${token.text}` : token.text;
    separated = false;
  }
  return text;
}

function validateQuery(query) {
  if (typeof query !== 'string') {
    throw new TypeError('Natural language query must be a string');
  }
  if (query.length > MAX_NATURAL_LANGUAGE_QUERY_LENGTH) {
    throw new RangeError(
      `Natural language query must be at most ${MAX_NATURAL_LANGUAGE_QUERY_LENGTH} characters (received ${query.length})`
    );
  }
}

/**
 * Parse natural language query into ServiceNow encoded query
 *
 * @param {string} query - Natural language query (at most 2048 characters)
 * @param {string} table - Target ServiceNow table (default: 'incident')
 * @returns {object} - { encodedQuery, matchedPatterns, unmatchedText, suggestions }
 * @throws {TypeError} when query is not a string
 * @throws {RangeError} when query exceeds MAX_NATURAL_LANGUAGE_QUERY_LENGTH
 */
export function parseNaturalLanguage(query, table = 'incident') {
  validateQuery(query);

  if (query.length === 0) {
    return {
      encodedQuery: '',
      matchedPatterns: [],
      unmatchedText: query,
      suggestions: ['Please provide a valid query string']
    };
  }

  const tokens = tokenize(query);
  const conditions = [];
  const matchedPatterns = [];
  const suggestions = [];

  for (const pattern of PATTERNS) {
    const match = pattern.find(tokens, table);
    if (match) {
      conditions.push(match.condition);
      matchedPatterns.push({
        pattern: pattern.id,
        matched: textOf(tokens, match.start, match.end),
        condition: match.condition
      });
      removeMatch(tokens, match.start, match.end);
    }
  }

  // Use OR if the remaining text contains "or", otherwise use AND (default)
  const hasOr = tokens.some((token) => isWord(token, 'or'));
  const encodedQuery = conditions.join(hasOr ? '^OR' : '^');
  const unmatchedText = describeUnmatched(tokens);

  // Generate suggestions for unmatched text
  if (unmatchedText.length > 3) {
    suggestions.push(`Unrecognized: "${unmatchedText}"`);
    suggestions.push('Try using encoded query format: field=value^field2=value2');
    suggestions.push('Supported patterns: priority (P1-P5), state (new/open/closed), assigned to me/unassigned, recent, dates');
  }

  // If no patterns matched, return original query (might be encoded query already)
  if (conditions.length === 0) {
    if (query.includes('=') || query.includes('^')) {
      return {
        encodedQuery: query,
        matchedPatterns: [],
        unmatchedText: '',
        suggestions: ['Using query as-is (appears to be encoded query format)']
      };
    }

    return {
      encodedQuery: '',
      matchedPatterns: [],
      unmatchedText: query,
      suggestions: [
        'No patterns matched. Supported patterns:',
        '- Priority: "high priority", "P1", "priority 2"',
        '- Assignment: "assigned to me", "unassigned", "assigned to John"',
        '- State: "new", "open", "closed", "in progress"',
        '- Dates: "created today", "last 7 days", "recent"',
        '- Content: "about SAP", "containing error"',
        'Or use ServiceNow encoded query format: field=value^field2=value2'
      ]
    };
  }

  return {
    encodedQuery,
    matchedPatterns,
    unmatchedText,
    suggestions: suggestions.length > 0 ? suggestions : ['Query parsed successfully']
  };
}

/**
 * Test the natural language parser with example queries
 *
 * @param {string} table - Table name to test against
 * @returns {Array} - Array of test results
 */
export function testParser(table = 'incident') {
  const testQueries = [
    'find all P1 incidents',
    'show recent problems assigned to me',
    'high priority changes created last week',
    'open incidents about SAP',
    'unassigned P2 incidents',
    'incidents created today with high priority',
    'closed problems assigned to John Smith',
    'critical incidents opened in the last 30 days',
    'show me my active tickets',
    'new incidents with high impact and high urgency',
    'incidents containing database error',
    'P1 incidents assigned to Network Team',
    'incidents created after January 1',
    'resolved incidents about authentication'
  ];

  return testQueries.map(query => ({
    query,
    result: parseNaturalLanguage(query, table)
  }));
}

/**
 * Get supported patterns documentation
 *
 * @returns {object} - Documentation of supported patterns
 */
export function getSupportedPatterns() {
  return {
    priority: {
      examples: ['high priority', 'P1', 'priority 2', 'critical priority'],
      encodedQuery: 'priority=1'
    },
    assignment: {
      examples: ['assigned to me', 'unassigned', 'assigned to John Smith', 'my incidents'],
      encodedQuery: 'assigned_to=javascript:gs.getUserID() or assigned_toISEMPTY'
    },
    state: {
      examples: ['new', 'open', 'active', 'in progress', 'closed', 'resolved'],
      encodedQuery: 'state=1 (varies by table)'
    },
    dates: {
      examples: ['created today', 'last 7 days', 'recent', 'opened yesterday', 'updated last week'],
      encodedQuery: 'sys_created_on>javascript:gs.daysAgo(7)'
    },
    content: {
      examples: ['about SAP', 'containing error', 'description contains authentication'],
      encodedQuery: 'short_descriptionLIKESAP'
    },
    impact: {
      examples: ['high impact', 'medium impact', 'low impact'],
      encodedQuery: 'impact=1'
    },
    urgency: {
      examples: ['high urgency', 'medium urgency', 'low urgency'],
      encodedQuery: 'urgency=1'
    },
    number: {
      examples: ['number is INC0012345'],
      encodedQuery: 'number=INC0012345'
    },
    caller: {
      examples: ['caller is John Smith'],
      encodedQuery: 'caller_id.nameLIKEJohn Smith'
    },
    category: {
      examples: ['category is Software'],
      encodedQuery: 'categoryLIKESoftware'
    },
    assignmentGroup: {
      examples: ['assignment group is Network Team'],
      encodedQuery: 'assignment_group.nameLIKENetwork Team'
    },
    combining: {
      examples: ['high priority and assigned to me', 'P1 or P2 incidents', 'recent and unassigned'],
      encodedQuery: 'Use "and" for ^ operator, "or" for ^OR operator'
    }
  };
}

export default {
  parseNaturalLanguage,
  testParser,
  getSupportedPatterns
};
