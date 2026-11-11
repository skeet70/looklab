-- SPDX-License-Identifier: AGPL-3.0-or-later

-- Untrusted client reports; the wire schema is validated before insert.
-- Rows carry no address, credential, message text or raw body.
CREATE TABLE client_errors (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    received_at INTEGER NOT NULL,
    release     TEXT NOT NULL,
    kind        TEXT NOT NULL
                        CHECK (kind IN ('console_error', 'exception',
                                        'unhandled_rejection', 'resource_error')),
    error_type  TEXT NOT NULL
                        CHECK (error_type IN ('Error', 'TypeError', 'RangeError',
                                              'ReferenceError', 'SyntaxError', 'URIError',
                                              'EvalError', 'RuntimeError', 'CompileError',
                                              'LinkError', 'Unknown')),
    page        TEXT NOT NULL
                        CHECK (page IN ('home', 'looks', 'look', 'compare', 'cameras',
                                        'camera', 'match', 'contribute', 'me', 'status',
                                        'community', 'moderation', 'about', 'other')),
    frames      TEXT NOT NULL
);

CREATE INDEX client_errors_by_received ON client_errors (received_at DESC, id DESC);
CREATE INDEX client_errors_by_release ON client_errors (release, received_at DESC);
