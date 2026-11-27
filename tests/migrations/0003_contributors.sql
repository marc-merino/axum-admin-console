-- Authors who contributed to a book: a many-to-many to integer keys.

CREATE TABLE library_book_contributors (
    id bigserial PRIMARY KEY,
    book_id uuid NOT NULL REFERENCES library_book (id),
    author_id bigint NOT NULL REFERENCES library_author (id),
    UNIQUE (book_id, author_id)
);
