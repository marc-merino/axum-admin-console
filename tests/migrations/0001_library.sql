-- A small library: integer-keyed authors, UUID-keyed books, text-keyed
-- tags, books tagged many-to-many, and reviews that protect their book.

CREATE TABLE library_author (
    id bigserial PRIMARY KEY,
    name text NOT NULL
);

CREATE TABLE library_tag (
    code text PRIMARY KEY,
    label text NOT NULL
);

CREATE TABLE library_book (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    title text NOT NULL,
    genre text NOT NULL DEFAULT 'novel',
    published date,
    author_id bigint NOT NULL REFERENCES library_author (id),
    editor_id bigint REFERENCES library_author (id)
);

CREATE TABLE library_book_tags (
    id bigserial PRIMARY KEY,
    book_id uuid NOT NULL REFERENCES library_book (id),
    tag_id text NOT NULL REFERENCES library_tag (code),
    UNIQUE (book_id, tag_id)
);

CREATE TABLE library_review (
    id bigserial PRIMARY KEY,
    book_id uuid NOT NULL REFERENCES library_book (id),
    body text NOT NULL
);

-- What the test history hook records.
CREATE TABLE console_history (
    id bigserial PRIMARY KEY,
    at timestamptz NOT NULL DEFAULT now(),
    staff text NOT NULL,
    app text NOT NULL,
    model text NOT NULL,
    key text NOT NULL,
    repr text NOT NULL,
    kind text NOT NULL,
    message text NOT NULL
);
