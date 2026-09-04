// Package store keeps synthetic records.
package store

import (
	"fmt"

	"example.com/demo/util"
)

// Limit caps the number of stored records.
const Limit = 16

// Record is one stored item.
type Record struct {
	ID int
}

// Reader is anything that can read a record.
type Reader interface {
	Read(id int) Record
}

// Store holds records.
type Store struct{}

// Put writes a record.
func (s *Store) Put(r Record) error {
	s.flush()
	return util.Check(r.ID)
}

func (s *Store) flush() {
	fmt.Println("flush")
}

// Open creates a store.
func Open() *Store {
	return &Store{}
}
