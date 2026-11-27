-- Loans of books, for column types beyond the library's: a Postgres enum, a
-- `timestamp without time zone` holding UTC, a uuid that is not a key, a
-- text array, jsonb and bytea.

CREATE TYPE library_loan_state AS ENUM ('open', 'returned', 'lost');

CREATE TABLE library_loan (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    book_id uuid NOT NULL REFERENCES library_book (id),
    state library_loan_state NOT NULL DEFAULT 'open',
    lent_at timestamp NOT NULL,
    updated_at timestamp NOT NULL DEFAULT (now() AT TIME ZONE 'UTC'),
    reader uuid,
    tags text[] NOT NULL DEFAULT '{}',
    notes jsonb NOT NULL DEFAULT '{}',
    digest bytea
);
