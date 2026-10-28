// The official Go Bolt driver against `mushroomdb serve --bolt-port`: connect
// with the token, run `MATCH (n) RETURN count(n)`, run one explicit
// transaction (begin, create, commit, read back), disconnect. Exits 1 on any
// error. Run through tests/bolt/extra_driver.sh, which fills @VENDOR@ in.
package main

import (
	"context"
	"fmt"
	"os"

	drv "github.com/@VENDOR@/@VENDOR@-go-driver/v6/@VENDOR@"
)

func fail(format string, args ...any) {
	fmt.Fprintf(os.Stderr, "go driver: "+format+"\n", args...)
	os.Exit(1)
}

func count(ctx context.Context, run func(string, map[string]any) (drv.ResultWithContext, error), query string) int64 {
	result, err := run(query, nil)
	if err != nil {
		fail("%v", err)
	}
	record, err := result.Single(ctx)
	if err != nil {
		fail("%v", err)
	}
	value, ok := record.Values[0].(int64)
	if !ok {
		fail("%s did not return an integer: %T", query, record.Values[0])
	}
	return value
}

func main() {
	ctx := context.Background()
	if len(os.Args) < 2 {
		fail("usage: go_compat <bolt-uri>")
	}
	token := os.Getenv("BOLT_COMPAT_TOKEN")
	driver, err := drv.NewDriverWithContext(os.Args[1], drv.BasicAuth("compat", token, ""))
	if err != nil {
		fail("%v", err)
	}
	defer driver.Close(ctx)
	if err := driver.VerifyConnectivity(ctx); err != nil {
		fail("%v", err)
	}

	session := driver.NewSession(ctx, drv.SessionConfig{})
	defer session.Close(ctx)
	count(ctx, func(q string, p map[string]any) (drv.ResultWithContext, error) {
		return session.Run(ctx, q, p)
	}, "MATCH (n) RETURN count(n)")

	tx, err := session.BeginTransaction(ctx)
	if err != nil {
		fail("%v", err)
	}
	if _, err := tx.Run(ctx, "CREATE (:Person {id: 'go-tx', name: 'Grace'})", nil); err != nil {
		fail("%v", err)
	}
	if err := tx.Commit(ctx); err != nil {
		fail("%v", err)
	}
	seen := count(ctx, func(q string, p map[string]any) (drv.ResultWithContext, error) {
		return session.Run(ctx, q, p)
	}, "MATCH (p:Person {id: 'go-tx'}) RETURN count(p)")
	if seen != 1 {
		fail("committed node not read back: %d", seen)
	}
	fmt.Println("go driver: connect, read and transaction passed")
}
