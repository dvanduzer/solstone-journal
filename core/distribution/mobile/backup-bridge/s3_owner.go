// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

import (
	"bytes"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"io"
	"net"
	"net/netip"
	"net/url"
	"os"
	"strconv"
	"strings"
	"sync"

	"golang.org/x/sys/unix"
)

const (
	s3OwnerStateName      = "state.json"
	s3OwnerTempPrefix     = ".s3-state-"
	maxS3OwnerBytes       = 128 * 1024
	maxS3OwnerCredentials = 4
	maxS3OwnerPasswords   = 16
	maxS3OwnerLocks       = 32
)

var (
	errS3OwnerInvalid = errors.New("S3_OWNER_STATE_INVALID")
	errS3OwnerIO      = errors.New("S3_OWNER_STATE_IO_FAILED")
	errS3OwnerPending = errors.New("S3_REMOTE_PENDING")
	errS3OwnerBusy    = errors.New("S3_DESTINATION_BUSY")
	errS3OwnerPolicy  = errors.New("S3_OWNER_FILE_POLICY_REFUSED")
	s3OwnersMu        sync.Mutex
	s3Owners          = make(map[s3Destination]*s3Owner)
)

type s3Destination struct {
	Endpoint string `json:"endpoint"`
	Region   string `json:"region"`
	Bucket   string `json:"bucket"`
	Prefix   string `json:"prefix"`
}

type s3RetainedCredential struct {
	Access string `json:"access"`
	Secret string `json:"secret"`
	Token  string `json:"token"`
}

type s3OwnerIntent struct {
	ID             string   `json:"id"`
	Operation      int32    `json:"operation"`
	MayHaveWritten bool     `json:"may_have_written"`
	Locks          []string `json:"locks"`
}

// A completed record is prepared only after observed write/lock outcomes and
// actual consequential work drain. Its visibility is not server quiescence.
type s3OwnerCompletion struct {
	ID        string `json:"id"`
	Operation int32  `json:"operation"`
}

type s3OwnerRecord struct {
	Version     int                    `json:"version"`
	Destination s3Destination          `json:"destination"`
	Credentials []s3RetainedCredential `json:"credentials"`
	Passwords   [][]byte               `json:"passwords"`
	Active      *s3OwnerIntent         `json:"active"`
	Complete    *s3OwnerCompletion     `json:"complete"`
}

type s3Owner struct {
	owned           *ownedCall
	mu              sync.Mutex
	path            string
	destination     s3Destination
	directory       *os.File
	root            *os.Root
	stateFile       *os.File
	record          s3OwnerRecord
	exists          bool
	closed          bool
	sealing         bool
	begun           bool
	primaryDrained  bool
	liveID          string
	cleanupActive   bool
	knownKeyID      string
	knownSnapshotID string
	writeFailed     bool
	stateFailed     bool
	lockAcks        map[string]bool
	backends        []*s3OperationBackend
	// Private deterministic fault/barrier injection; no public setter or ABI.
	fault func(string) error
}

func s3DestinationFor(cfg s3Config) (s3Destination, error) {
	if cfg.validate() != nil {
		return s3Destination{}, errS3Config
	}
	parsed, _ := url.Parse(cfg.endpoint)
	port := parsed.Port()
	if port == "" {
		port = "443"
	}
	number, _ := strconv.ParseUint(port, 10, 16)
	host := strings.ToLower(parsed.Hostname())
	if address, err := netip.ParseAddr(host); err == nil {
		host = address.String()
	}
	endpoint := "https://" + net.JoinHostPort(host, strconv.FormatUint(number, 10))
	return s3Destination{endpoint, cfg.region, cfg.bucket, cfg.prefix}, nil
}

func (destination s3Destination) config(credential s3RetainedCredential) s3Config {
	return s3Config{endpoint: destination.Endpoint, region: destination.Region, bucket: destination.Bucket,
		prefix: destination.Prefix, access: credential.Access, secret: credential.Secret, token: credential.Token}
}

func newS3Owner(path string, cfg s3Config) (_ *s3Owner, resultErr error) {
	return newS3OwnerOwned(path, cfg, nil)
}

func newS3OwnerOwned(path string, cfg s3Config, entry *ownedCall) (_ *s3Owner, resultErr error) {
	destination, err := s3DestinationFor(cfg)
	if err != nil || sanitizePath(path) != nil {
		return nil, errS3OwnerInvalid
	}
	owner := &s3Owner{owned: entry, path: path, destination: destination, lockAcks: make(map[string]bool)}
	entry.retainOwner(owner)
	s3OwnersMu.Lock()
	if s3Owners[destination] != nil {
		s3OwnersMu.Unlock()
		return nil, errS3OwnerBusy
	}
	s3Owners[destination] = owner
	s3OwnersMu.Unlock()
	defer func() {
		if resultErr != nil {
			entry.scrubRejection()
			_ = owner.Close()
		}
	}()
	file, err := openOwnedDirectory(path, entry)
	if err != nil {
		return nil, errS3OwnerInvalid
	}
	owner.directory = file
	var stat unix.Stat_t
	if unix.Fstat(int(file.Fd()), &stat) != nil || stat.Uid != uint32(os.Geteuid()) || stat.Mode&07777 != 0700 {
		return nil, errS3OwnerInvalid
	}
	if err := reserveOwnedPrivateDirectory(file, entry); err != nil {
		return nil, err
	}
	if err := unix.Flock(int(file.Fd()), unix.LOCK_EX|unix.LOCK_NB); err != nil {
		if errors.Is(err, unix.EWOULDBLOCK) || errors.Is(err, unix.EAGAIN) {
			return nil, errS3OwnerBusy
		}
		return nil, errS3OwnerIO
	}
	if err := s3StateRootPolicy(int(file.Fd())); err != nil {
		return nil, errS3OwnerPolicy
	}
	owner.root, err = adoptOwnedRoot(path, file, entry)
	if err != nil {
		return nil, errS3OwnerInvalid
	}
	if err := owner.census(""); err != nil {
		return nil, err
	}
	if err := owner.read(); err != nil {
		return nil, err
	}
	return owner, nil
}

func (owner *s3Owner) gate(stage string) error {
	if owner.fault != nil {
		return owner.fault(stage)
	}
	return nil
}

// No residue is deleted here. An interrupted private temp is explicit custody,
// rather than permission to reset a record or discard credential material.
func (owner *s3Owner) census(temporary string) error {
	fd, err := unix.Openat(int(owner.directory.Fd()), ".", unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC, 0)
	if err != nil {
		return errS3OwnerIO
	}
	listing := os.NewFile(uintptr(fd), "S3 owner state census")
	names, readErr := listing.Readdirnames(10)
	if owner.owned != nil {
		invalid := readErr != nil && !errors.Is(readErr, io.EOF) || len(names) > 2
		for _, name := range names {
			invalid = invalid || name != s3OwnerStateName && (temporary == "" || name != temporary)
		}
		if invalid {
			owner.owned.scrubRejection()
		}
	}
	closeErr := owner.owned.close(listing, "s3-state-census")
	if readErr != nil && !errors.Is(readErr, io.EOF) || closeErr != nil || len(names) > 2 {
		return errS3OwnerInvalid
	}
	for _, name := range names {
		if name != s3OwnerStateName && (temporary == "" || name != temporary) {
			return errS3OwnerInvalid
		}
	}
	return nil
}

func privateS3StateFile(file *os.File) bool {
	var stat unix.Stat_t
	return file != nil && unix.Fstat(int(file.Fd()), &stat) == nil && stat.Mode&unix.S_IFMT == unix.S_IFREG &&
		stat.Mode&07777 == 0600 && stat.Uid == uint32(os.Geteuid()) && stat.Nlink == 1 && stat.Size >= 0 && stat.Size <= maxS3OwnerBytes
}

func (owner *s3Owner) read() error {
	fd, err := unix.Openat(int(owner.directory.Fd()), s3OwnerStateName, unix.O_RDONLY|unix.O_NOFOLLOW|unix.O_CLOEXEC|unix.O_NONBLOCK, 0)
	if errors.Is(err, unix.ENOENT) {
		return nil
	}
	if err != nil {
		return errS3OwnerInvalid
	}
	file := os.NewFile(uintptr(fd), "S3 owner state")
	keep := false
	defer func() {
		if !keep {
			owner.owned.scrubRejection()
			_ = owner.owned.close(file, "s3-state-rejected-read")
		}
	}()
	if !privateS3StateFile(file) || s3StateFilePolicy(fd, false) != nil {
		return errS3OwnerInvalid
	}
	raw, err := io.ReadAll(io.LimitReader(file, maxS3OwnerBytes+1))
	defer zeroBytes(raw)
	if err != nil || len(raw) > maxS3OwnerBytes || uniqueS3StateJSON(raw) != nil || completeS3StateJSON(raw) != nil {
		return errS3OwnerInvalid
	}
	var record s3OwnerRecord
	decoder := json.NewDecoder(bytes.NewReader(raw))
	decoder.DisallowUnknownFields()
	if decoder.Decode(&record) != nil || record.validate() != nil || record.Destination != owner.destination {
		return errS3OwnerInvalid
	}
	if matches, err := pinnedEntryMatches(int(owner.directory.Fd()), s3OwnerStateName, file); err != nil || !matches {
		return errS3OwnerInvalid
	}
	owner.stateFile, owner.record, owner.exists = file, record, true
	keep = true
	return nil
}

func uniqueS3StateJSON(raw []byte) error {
	decoder := json.NewDecoder(bytes.NewReader(raw))
	var value func(int) error
	value = func(depth int) error {
		if depth > 12 {
			return errS3OwnerInvalid
		}
		token, err := decoder.Token()
		if err != nil {
			return errS3OwnerInvalid
		}
		delimiter, nested := token.(json.Delim)
		if !nested {
			return nil
		}
		switch delimiter {
		case '{':
			seen := make(map[string]bool)
			for decoder.More() {
				key, err := decoder.Token()
				if err != nil {
					return errS3OwnerInvalid
				}
				name, ok := key.(string)
				if !ok || seen[name] || len(seen) >= 32 {
					return errS3OwnerInvalid
				}
				seen[name] = true
				if err := value(depth + 1); err != nil {
					return err
				}
			}
			end, err := decoder.Token()
			if err != nil || end != json.Delim('}') {
				return errS3OwnerInvalid
			}
		case '[':
			count := 0
			for decoder.More() {
				count++
				if count > 32 {
					return errS3OwnerInvalid
				}
				if err := value(depth + 1); err != nil {
					return err
				}
			}
			end, err := decoder.Token()
			if err != nil || end != json.Delim(']') {
				return errS3OwnerInvalid
			}
		default:
			return errS3OwnerInvalid
		}
		return nil
	}
	if err := value(0); err != nil {
		return err
	}
	if _, err := decoder.Token(); !errors.Is(err, io.EOF) {
		return errS3OwnerInvalid
	}
	return nil
}

// Missing/null booleans cannot default an interrupted write to "no egress".
// Exact field presence complements duplicate rejection and typed validation.
func completeS3StateJSON(raw []byte) error {
	object := func(raw []byte, names []string, nullable map[string]bool) (map[string]json.RawMessage, error) {
		var fields map[string]json.RawMessage
		if json.Unmarshal(raw, &fields) != nil || len(fields) != len(names) {
			return nil, errS3OwnerInvalid
		}
		for _, name := range names {
			value, present := fields[name]
			if !present || bytes.Equal(bytes.TrimSpace(value), []byte("null")) && !nullable[name] {
				return nil, errS3OwnerInvalid
			}
		}
		return fields, nil
	}
	fields, err := object(raw, []string{"version", "destination", "credentials", "passwords", "active", "complete"}, map[string]bool{"active": true, "complete": true})
	if err != nil {
		return err
	}
	if _, err := object(fields["destination"], []string{"endpoint", "region", "bucket", "prefix"}, nil); err != nil {
		return err
	}
	var credentials []json.RawMessage
	if json.Unmarshal(fields["credentials"], &credentials) != nil || len(credentials) > maxS3OwnerCredentials {
		return errS3OwnerInvalid
	}
	for _, credential := range credentials {
		if _, err := object(credential, []string{"access", "secret", "token"}, nil); err != nil {
			return err
		}
	}
	if !bytes.Equal(bytes.TrimSpace(fields["active"]), []byte("null")) {
		if _, err := object(fields["active"], []string{"id", "operation", "may_have_written", "locks"}, map[string]bool{"locks": true}); err != nil {
			return err
		}
	}
	if !bytes.Equal(bytes.TrimSpace(fields["complete"]), []byte("null")) {
		if _, err := object(fields["complete"], []string{"id", "operation"}, nil); err != nil {
			return err
		}
	}
	return nil
}

func (record s3OwnerRecord) validate() error {
	if record.Version != 1 || len(record.Credentials) == 0 || len(record.Credentials) > maxS3OwnerCredentials || len(record.Passwords) == 0 || len(record.Passwords) > maxS3OwnerPasswords || (record.Active == nil) == (record.Complete == nil) {
		return errS3OwnerInvalid
	}
	for i, credential := range record.Credentials {
		cfg := record.Destination.config(credential)
		destination, err := s3DestinationFor(cfg)
		if err != nil || destination != record.Destination {
			return errS3OwnerInvalid
		}
		for _, previous := range record.Credentials[:i] {
			if previous == credential {
				return errS3OwnerInvalid
			}
		}
	}
	for i, password := range record.Passwords {
		if len(password) == 0 || len(password) > maxPasswordLen {
			return errS3OwnerInvalid
		}
		for _, previous := range record.Passwords[:i] {
			if bytes.Equal(previous, password) {
				return errS3OwnerInvalid
			}
		}
	}
	if active := record.Active; active != nil {
		if !isMetadataNonce(active.ID) || active.Operation < OpInit || active.Operation > OpCheck || len(active.Locks) > maxS3OwnerLocks || len(active.Locks) != 0 && !active.MayHaveWritten {
			return errS3OwnerInvalid
		}
		seen := make(map[string]bool)
		for _, id := range active.Locks {
			if !validS3ID(id) || seen[id] {
				return errS3OwnerInvalid
			}
			seen[id] = true
		}
	}
	if complete := record.Complete; complete != nil {
		if !isMetadataNonce(complete.ID) || complete.Operation < OpInit || complete.Operation > OpCheck {
			return errS3OwnerInvalid
		}
	}
	return nil
}

func (owner *s3Owner) recheckDirectory() (resultErr error) {
	file, err := openOwnedDirectory(owner.path, owner.owned)
	if err != nil {
		return errS3OwnerInvalid
	}
	defer func() {
		err := owner.owned.close(file, "s3-state-recheck-directory")
		if owner.owned != nil {
			resultErr = errors.Join(resultErr, err)
		}
	}()
	same, err := sameDirectory(file, owner.directory)
	if err != nil || !same {
		return errS3OwnerInvalid
	}
	var stat unix.Stat_t
	if unix.Fstat(int(owner.directory.Fd()), &stat) != nil || stat.Uid != uint32(os.Geteuid()) || stat.Mode&07777 != 0700 || s3StateRootPolicy(int(owner.directory.Fd())) != nil {
		return errS3OwnerInvalid
	}
	return nil
}

func (owner *s3Owner) recheckCustody(temporary string) error {
	if owner.recheckDirectory() != nil || owner.census(temporary) != nil {
		return errS3OwnerInvalid
	}
	if owner.stateFile != nil {
		if !privateS3StateFile(owner.stateFile) || s3StateFilePolicy(int(owner.stateFile.Fd()), false) != nil {
			return errS3OwnerInvalid
		}
		if matches, err := pinnedEntryMatches(int(owner.directory.Fd()), s3OwnerStateName, owner.stateFile); err != nil || !matches {
			return errS3OwnerInvalid
		}
	} else {
		var stat unix.Stat_t
		if err := unix.Fstatat(int(owner.directory.Fd()), s3OwnerStateName, &stat, unix.AT_SYMLINK_NOFOLLOW); !errors.Is(err, unix.ENOENT) {
			return errS3OwnerInvalid
		}
	}
	return nil
}

// persist reports rename visibility separately from its final sync acknowledgement.
// The caller holds owner.mu. No payload is written before actual file-policy checks.
func (owner *s3Owner) persist(record s3OwnerRecord) (published bool, resultErr error) {
	if record.validate() != nil || owner.recheckCustody("") != nil {
		return false, errS3OwnerInvalid
	}
	encoded, err := json.Marshal(record)
	if err != nil || len(encoded) > maxS3OwnerBytes {
		return false, errResourceLimit
	}
	defer zeroBytes(encoded)
	var nonce [16]byte
	if _, err := rand.Read(nonce[:]); err != nil {
		return false, errS3OwnerIO
	}
	name := s3OwnerTempPrefix + hex.EncodeToString(nonce[:]) + ".tmp"
	fd, err := unix.Openat(int(owner.directory.Fd()), name, unix.O_WRONLY|unix.O_CREAT|unix.O_EXCL|unix.O_NOFOLLOW|unix.O_CLOEXEC, 0600)
	if err != nil {
		return false, errS3OwnerIO
	}
	file := os.NewFile(uintptr(fd), "S3 owner state temporary")
	defer func() { _ = owner.owned.close(file, "s3-state-write-temporary") }()
	if !privateS3StateFile(file) || s3StateFilePolicy(fd, true) != nil {
		return false, errS3OwnerPolicy
	}
	if owner.gate("before-secret-write") != nil {
		return false, errS3OwnerIO
	}
	if n, err := file.Write(encoded); err != nil || n != len(encoded) {
		return false, errS3OwnerIO
	}
	if owner.gate("file-sync") != nil || s3StateSyncFile(fd) != nil {
		return false, errS3OwnerIO
	}
	if owner.gate("after-file-sync") != nil {
		return false, errS3OwnerIO
	}
	readFD, err := unix.Openat(int(owner.directory.Fd()), name, unix.O_RDONLY|unix.O_NOFOLLOW|unix.O_CLOEXEC|unix.O_NONBLOCK, 0)
	if err != nil {
		return false, errS3OwnerIO
	}
	identity := os.NewFile(uintptr(readFD), "S3 owner state identity")
	keepIdentity := false
	defer func() {
		if !keepIdentity {
			_ = owner.owned.close(identity, "s3-state-read-identity")
		}
	}()
	if same, err := sameDirectory(file, identity); err != nil || !same || !privateS3StateFile(identity) {
		return false, errS3OwnerInvalid
	}
	if err := owner.owned.close(file, "s3-state-write-temporary"); err != nil {
		return false, errS3OwnerIO
	}
	if owner.gate("before-rename") != nil || owner.recheckCustody(name) != nil {
		return false, errS3OwnerIO
	}
	if matches, err := pinnedEntryMatches(int(owner.directory.Fd()), name, identity); err != nil || !matches {
		return false, errS3OwnerInvalid
	}
	if err := owner.gate("atomic-replace"); err != nil {
		return false, errS3OwnerIO
	}
	if err := s3StateReplace(int(owner.directory.Fd()), name, s3OwnerStateName, owner.stateFile != nil); err != nil {
		return false, errS3OwnerIO
	}
	if owner.stateFile != nil {
		// Exchange preserves the displaced inode even if a nonconforming same-UID
		// writer changed the target in the final check/syscall interval.
		matches, err := pinnedEntryMatches(int(owner.directory.Fd()), name, owner.stateFile)
		newMatches, newErr := pinnedEntryMatches(int(owner.directory.Fd()), s3OwnerStateName, identity)
		if err != nil || !matches || newErr != nil || !newMatches || !privateS3StateFile(identity) || s3StateFilePolicy(readFD, false) != nil || !privateS3StateFile(owner.stateFile) || s3StateFilePolicy(int(owner.stateFile.Fd()), false) != nil {
			_ = unix.Fsync(int(owner.directory.Fd()))
			return true, errS3OwnerInvalid
		}
		if err := unix.Unlinkat(int(owner.directory.Fd()), name, 0); err != nil {
			_ = unix.Fsync(int(owner.directory.Fd()))
			return true, errS3OwnerIO
		}
		if err := owner.owned.close(owner.stateFile, "s3-state-displaced-file"); err != nil && owner.owned != nil {
			return true, err
		}
	}
	owner.stateFile, owner.record, owner.exists = identity, record, true
	keepIdentity = true
	published = true
	if matches, err := pinnedEntryMatches(int(owner.directory.Fd()), s3OwnerStateName, identity); err != nil || !matches || s3StateFilePolicy(readFD, false) != nil {
		return true, errS3OwnerInvalid
	}
	if owner.gate("directory-sync") != nil || unix.Fsync(int(owner.directory.Fd())) != nil {
		return true, errS3OwnerIO
	}
	return true, nil
}

func (owner *s3Owner) begin(operation int32, cfg s3Config, passwords ...[]byte) error {
	owner.mu.Lock()
	defer owner.mu.Unlock()
	if owner.closed || owner.sealing || operation < OpInit || operation > OpCheck {
		return errS3OwnerInvalid
	}
	if owner.record.Active != nil && owner.record.Active.MayHaveWritten {
		return errS3OwnerPending
	}
	destination, err := s3DestinationFor(cfg)
	if err != nil || destination != owner.destination {
		return errS3OwnerInvalid
	}
	record := owner.record
	record.Version, record.Destination = 1, owner.destination
	record.Credentials = append([]s3RetainedCredential(nil), record.Credentials...)
	record.Passwords = append([][]byte(nil), record.Passwords...)
	credential := s3RetainedCredential{cfg.access, cfg.secret, cfg.token}
	found := false
	for _, retained := range record.Credentials {
		if retained == credential {
			found = true
		}
	}
	if !found {
		record.Credentials = append(record.Credentials, credential)
	}
	for _, password := range passwords {
		if len(password) == 0 {
			continue
		}
		found = false
		for _, retained := range record.Passwords {
			if bytes.Equal(retained, password) {
				found = true
			}
		}
		if !found {
			record.Passwords = append(record.Passwords, append([]byte(nil), password...))
		}
	}
	var nonce [16]byte
	if _, err := rand.Read(nonce[:]); err != nil {
		return errS3OwnerIO
	}
	record.Active = &s3OwnerIntent{ID: hex.EncodeToString(nonce[:]), Operation: operation, Locks: []string{}}
	record.Complete = nil
	if len(record.Credentials) > maxS3OwnerCredentials || len(record.Passwords) > maxS3OwnerPasswords {
		return errResourceLimit
	}
	_, err = owner.persist(record)
	owner.begun = true
	if err != nil {
		owner.stateFailed = true
	} else {
		owner.liveID = record.Active.ID
	}
	return err
}

// noteSave is write-ahead admission, not a claim that any request was sent.
func (owner *s3Owner) noteSave(lockID string) error {
	owner.mu.Lock()
	defer owner.mu.Unlock()
	if owner.closed || owner.sealing || owner.stateFailed || owner.record.Active == nil || owner.liveID == "" || owner.liveID != owner.record.Active.ID {
		return errS3OwnerInvalid
	}
	if owner.recheckCustody("") != nil {
		owner.stateFailed = true
		return errS3OwnerInvalid
	}
	record := owner.record
	active := *record.Active
	active.Locks = append([]string(nil), active.Locks...)
	changed := !active.MayHaveWritten
	active.MayHaveWritten = true
	if lockID != "" {
		if !validS3ID(lockID) {
			return errS3OwnerInvalid
		}
		found := false
		for _, id := range active.Locks {
			if id == lockID {
				found = true
			}
		}
		if !found {
			if len(active.Locks) >= maxS3OwnerLocks {
				return errResourceLimit
			}
			active.Locks = append(active.Locks, lockID)
			changed = true
		}
	}
	if !changed {
		return nil
	}
	record.Active = &active
	_, err := owner.persist(record)
	if err != nil {
		owner.stateFailed = true
	}
	return err
}

func (owner *s3Owner) saveReturned(err error) {
	if err != nil {
		owner.mu.Lock()
		owner.writeFailed = true
		owner.mu.Unlock()
	}
}

func (owner *s3Owner) ownsLock(id string) bool {
	owner.mu.Lock()
	defer owner.mu.Unlock()
	if owner.record.Active == nil {
		return false
	}
	for _, owned := range owner.record.Active.Locks {
		if owned == id {
			return true
		}
	}
	return false
}

func (owner *s3Owner) lockRemoved(id string, err error) {
	if err == nil {
		owner.mu.Lock()
		owner.lockAcks[id] = true
		owner.mu.Unlock()
	}
}

// Call only after actual consequential operation/backend work and cleanup drain.
func (owner *s3Owner) complete() (published bool, err error) {
	owner.mu.Lock()
	defer owner.mu.Unlock()
	if owner.record.Active == nil || owner.closed || !owner.primaryDrained || owner.cleanupActive || owner.liveID == "" || owner.liveID != owner.record.Active.ID {
		return false, errS3OwnerInvalid
	}
	if owner.writeFailed || owner.stateFailed {
		return false, errS3OwnerPending
	}
	for _, id := range owner.record.Active.Locks {
		if !owner.lockAcks[id] {
			return false, errS3OwnerPending
		}
	}
	record := owner.record
	record.Complete = &s3OwnerCompletion{ID: record.Active.ID, Operation: record.Active.Operation}
	record.Active = nil
	return owner.persist(record)
}

func (owner *s3Owner) Close() error {
	if owner.owned != nil {
		return owner.closeOwned()
	}
	owner.mu.Lock()
	if owner.closed {
		owner.mu.Unlock()
		return nil
	}
	if owner.begun && !owner.primaryDrained || owner.cleanupActive {
		owner.mu.Unlock()
		return errS3OwnerBusy
	}
	owner.closed = true
	var closeErr error
	if owner.stateFile != nil {
		closeErr = errors.Join(closeErr, owner.stateFile.Close())
		owner.stateFile = nil
	}
	if owner.root != nil {
		closeErr = errors.Join(closeErr, owner.root.Close())
		owner.root = nil
	}
	if owner.directory != nil {
		closeErr = errors.Join(closeErr, closeRootedPrivateDirectory(owner.directory))
		owner.directory = nil
	}
	for _, password := range owner.record.Passwords {
		zeroBytes(password)
	}
	owner.mu.Unlock()
	s3OwnersMu.Lock()
	if s3Owners[owner.destination] == owner {
		delete(s3Owners, owner.destination)
	}
	s3OwnersMu.Unlock()
	return closeErr
}
