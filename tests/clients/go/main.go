// pgx against the proxy.
//
// pgx is a fourth independent implementation of the protocol, and the one most
// committed to binary format: it describes statements, caches them, and decodes
// nearly everything as binary. If the proxy's binary encoding were subtly wrong
// anywhere, this is where it would show.
//
// Usage: PGPORT=55432 go run .
package main

import (
	"context"
	"errors"
	"fmt"
	"os"
	"time"

	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgconn"
)

var failures int

func check(name string, fn func() error) {
	if err := fn(); err != nil {
		failures++
		fmt.Printf("FAIL %s\n     %v\n", name, err)
		return
	}
	fmt.Printf("ok   %s\n", name)
}

func main() {
	port := os.Getenv("PGPORT")
	if port == "" {
		port = "55432"
	}
	database := os.Getenv("DATABASE")
	if database == "" {
		database = "postgres"
	}
	ctx := context.Background()
	table := fmt.Sprintf("go_test_%d", os.Getpid())

	conn, err := pgx.Connect(ctx, fmt.Sprintf(
		"host=127.0.0.1 port=%s user=test password=anything dbname=%s", port, database))
	if err != nil {
		fmt.Println("could not connect:", err)
		os.Exit(1)
	}
	defer conn.Close(ctx)

	check("simple query", func() error {
		var n int32
		if err := conn.QueryRow(ctx, "SELECT 1").Scan(&n); err != nil {
			return err
		}
		if n != 1 {
			return fmt.Errorf("got %d", n)
		}
		return nil
	})

	check("parameterised query infers its types", func() error {
		var n int32
		// pgx sends Parse with no types and relies on Describe to learn them.
		if err := conn.QueryRow(ctx, "SELECT $1::int + 1", 41).Scan(&n); err != nil {
			return err
		}
		if n != 42 {
			return fmt.Errorf("got %d", n)
		}
		return nil
	})

	check("setup", func() error {
		_, err := conn.Exec(ctx, fmt.Sprintf(
			`CREATE TABLE %s (id serial PRIMARY KEY, name text NOT NULL,
			   amount numeric(12,2), ok boolean, tags text[], created_at timestamptz DEFAULT now())`,
			table))
		return err
	})

	check("insert with parameters runs exactly once", func() error {
		tag, err := conn.Exec(ctx,
			fmt.Sprintf("INSERT INTO %s(name, ok) VALUES ($1, $2)", table), "alpha", true)
		if err != nil {
			return err
		}
		if tag.RowsAffected() != 1 {
			return fmt.Errorf("rows affected = %d", tag.RowsAffected())
		}
		var n int64
		if err := conn.QueryRow(ctx,
			fmt.Sprintf("SELECT count(*) FROM %s", table)).Scan(&n); err != nil {
			return err
		}
		if n != 1 {
			return fmt.Errorf("the insert ran %d times", n)
		}
		return nil
	})

	check("insert ... returning runs exactly once", func() error {
		var id int32
		var name string
		err := conn.QueryRow(ctx,
			fmt.Sprintf("INSERT INTO %s(name) VALUES ($1) RETURNING id, name", table), "beta").
			Scan(&id, &name)
		if err != nil {
			return err
		}
		if name != "beta" {
			return fmt.Errorf("got %q", name)
		}
		var n int64
		if err := conn.QueryRow(ctx,
			fmt.Sprintf("SELECT count(*) FROM %s", table)).Scan(&n); err != nil {
			return err
		}
		if n != 2 {
			return fmt.Errorf("the table has %d rows", n)
		}
		return nil
	})

	check("binary types survive the round trip", func() error {
		var (
			b   bool
			i2  int16
			i4  int32
			i8  int64
			f8  float64
			s   string
			by  []byte
			arr []int32
			nul *string
		)
		err := conn.QueryRow(ctx,
			`SELECT true, 42::int2, 42::int4, 42::int8, 1.5::float8, 'hi'::text,
			        decode('deadbeef','hex'), ARRAY[1,2,3]::int4[], NULL::text`).
			Scan(&b, &i2, &i4, &i8, &f8, &s, &by, &arr, &nul)
		if err != nil {
			return err
		}
		if !b || i2 != 42 || i4 != 42 || i8 != 42 || f8 != 1.5 || s != "hi" {
			return fmt.Errorf("scalar mismatch")
		}
		if len(by) != 4 || by[0] != 0xde {
			return fmt.Errorf("bytea mismatch: %v", by)
		}
		if len(arr) != 3 || arr[2] != 3 {
			return fmt.Errorf("array mismatch: %v", arr)
		}
		if nul != nil {
			return fmt.Errorf("expected NULL")
		}
		return nil
	})

	check("timestamptz keeps its instant", func() error {
		var ts time.Time
		if err := conn.QueryRow(ctx,
			"SELECT '2024-01-15 12:34:56+09'::timestamptz").Scan(&ts); err != nil {
			return err
		}
		want := time.Date(2024, 1, 15, 3, 34, 56, 0, time.UTC)
		if !ts.Equal(want) {
			return fmt.Errorf("got %s, want %s", ts, want)
		}
		return nil
	})

	check("transaction commits and rolls back", func() error {
		tx, err := conn.Begin(ctx)
		if err != nil {
			return err
		}
		if _, err := tx.Exec(ctx,
			fmt.Sprintf("INSERT INTO %s(name) VALUES ($1)", table), "kept"); err != nil {
			return err
		}
		if err := tx.Commit(ctx); err != nil {
			return err
		}

		tx, err = conn.Begin(ctx)
		if err != nil {
			return err
		}
		if _, err := tx.Exec(ctx,
			fmt.Sprintf("INSERT INTO %s(name) VALUES ($1)", table), "dropped"); err != nil {
			return err
		}
		if err := tx.Rollback(ctx); err != nil {
			return err
		}

		var n int64
		if err := conn.QueryRow(ctx,
			fmt.Sprintf("SELECT count(*) FROM %s WHERE name = 'dropped'", table)).Scan(&n); err != nil {
			return err
		}
		if n != 0 {
			return fmt.Errorf("rollback left %d rows", n)
		}
		return nil
	})

	check("errors carry their sqlstate", func() error {
		_, err := conn.Exec(ctx, "SELECT * FROM no_such_table_here")
		if err == nil {
			return fmt.Errorf("expected an error")
		}
		var pgErr *pgconn.PgError
		if !errors.As(err, &pgErr) {
			return fmt.Errorf("not a PgError: %v", err)
		}
		if pgErr.Code != "42P01" {
			return fmt.Errorf("sqlstate = %q", pgErr.Code)
		}
		return nil
	})

	check("teardown", func() error {
		_, err := conn.Exec(ctx, fmt.Sprintf("DROP TABLE %s", table))
		return err
	})

	if failures == 0 {
		fmt.Println("\nall pgx checks passed")
		return
	}
	fmt.Printf("\n%d failed\n", failures)
	os.Exit(1)
}
