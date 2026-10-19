// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

/*
#include <stdint.h>
*/
import "C"

import (
	"bytes"
	"crypto/rand"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"io"
	"os"
	"sort"
	"unsafe"

	"golang.org/x/sys/unix"
)

// Descriptive evidence only. The caller must retain the actual locked directory.
// It contains no credential, password or reversible encoding of either.
type s3TeardownObservation struct {
	Version    int    `json:"version"`
	Present    bool   `json:"present"`
	Device     uint64 `json:"device"`
	Inode      uint64 `json:"inode"`
	ID         string `json:"id"`
	Operation  int32  `json:"operation"`
	Active     bool   `json:"active"`
	Scope      string `json:"scope"`
	Origin     string `json:"origin"`
	Reconciled bool   `json:"reconciled"`
}

type s3TeardownAdmission struct {
	ConfigSHA    string                `json:"config_sha256"`
	RepositoryID string                `json:"repository_id"`
	Origin       string                `json:"origin"`
	Expected     s3TeardownObservation `json:"expected"`
}

func strictTeardownJSON(raw []byte, out any) error {
	if len(raw) == 0 || len(raw) > 4096 || uniqueS3StateJSON(raw) != nil {
		return errS3OwnerInvalid
	}
	decoder := json.NewDecoder(bytes.NewReader(raw))
	decoder.DisallowUnknownFields()
	if decoder.Decode(out) != nil {
		return errS3OwnerInvalid
	}
	return nil
}

func teardownHex(value string, size int) bool { return len(value) == size && lowerHexID(value) }

func (ob s3TeardownObservation) valid() bool {
	if ob.Version != 1 {
		return false
	}
	if !ob.Present {
		return ob.Device == 0 && ob.Inode == 0 && ob.ID == "" && ob.Operation == 0 && !ob.Active && ob.Scope == "" && ob.Origin == "" && !ob.Reconciled
	}
	if ob.Inode == 0 || !isMetadataNonce(ob.ID) || !validOwnerOperation(ob.Operation) {
		return false
	}
	if ob.Scope == "" && ob.Origin == "" {
		return !ob.Active && ob.Operation != OpTeardown && !ob.Reconciled
	}
	return ob.Operation == OpTeardown && teardownHex(ob.Scope, 64) && isMetadataNonce(ob.Origin) && (!ob.Reconciled || !ob.Active)
}

func (ad s3TeardownAdmission) scope(destination s3Destination, ids []string) (string, error) {
	if len(ad.ConfigSHA) != 71 || ad.ConfigSHA[:7] != "sha256:" || !teardownHex(ad.ConfigSHA[7:], 64) || !teardownHex(ad.RepositoryID, 64) || !isMetadataNonce(ad.Origin) || !ad.Expected.valid() || !ad.Expected.Present {
		return "", errS3OwnerInvalid
	}
	ordered := append([]string(nil), ids...)
	sort.Strings(ordered)
	if validateTeardownShape(OpTeardown, 0, 0, 1, 0, 0, uint32(len(ids)), ids) != StatusOk {
		return "", errS3OwnerInvalid
	}
	digest := sha256.New()
	digest.Write([]byte("solstone.ios-teardown.v1\x00"))
	for _, value := range append([]string{ad.ConfigSHA, destination.Endpoint, destination.Region, destination.Bucket, destination.Prefix, ad.RepositoryID, ad.Origin}, ordered...) {
		digest.Write([]byte(value))
		digest.Write([]byte{0})
	}
	scope := hex.EncodeToString(digest.Sum(nil))
	if ad.Expected.Operation == OpTeardown {
		if ad.Expected.Scope != scope || ad.Expected.Origin != ad.Origin {
			return "", errS3OwnerPending
		}
	} else if ad.Expected.ID != ad.Origin || ad.Expected.Active {
		return "", errS3OwnerPending
	}
	return scope, nil
}

func teardownDirectoryPolicy(fd int) error {
	var st unix.Stat_t
	if unix.Fstat(fd, &st) != nil || st.Mode&unix.S_IFMT != unix.S_IFDIR || st.Mode&07777 != 0700 || st.Uid != uint32(os.Geteuid()) || s3StateRootPolicy(fd) != nil {
		return errS3OwnerInvalid
	}
	return nil
}

func teardownStateCensus(fd int) ([]string, error) {
	copyFD, err := unix.Openat(fd, ".", unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC, 0)
	if err != nil {
		return nil, err
	}
	file := os.NewFile(uintptr(copyFD), "teardown state census")
	names, readErr := file.Readdirnames(3)
	closeErr := file.Close()
	if readErr != nil && !errors.Is(readErr, io.EOF) {
		return nil, readErr
	}
	if closeErr != nil || len(names) > 1 || (len(names) == 1 && names[0] != s3OwnerStateName) {
		return nil, errS3OwnerInvalid
	}
	return names, nil
}

func eraseTeardownRecord(record *s3OwnerRecord) {
	for _, password := range record.Passwords {
		zeroBytes(password)
	}
	if record.Active != nil {
		zeroBytes(record.Active.RecoveryPassword)
		zeroBytes(record.Active.Ciphertext)
	}
	if record.RecoveryHold != nil {
		zeroBytes(record.RecoveryHold.RecoveryPassword)
		zeroBytes(record.RecoveryHold.Ciphertext)
	}
}

// No create, policy repair, owner registration, state rewrite or network work.
func observeTeardownState(fd int, destination s3Destination, pendingScope string) (ob s3TeardownObservation, file *os.File, resultErr error) {
	if err := teardownDirectoryPolicy(fd); err != nil {
		return ob, nil, err
	}
	normalized, err := s3DestinationFor(destination.config(s3RetainedCredential{Access: "observation", Secret: "observation"}))
	if err != nil || normalized != destination {
		return ob, nil, errS3OwnerInvalid
	}
	names, err := teardownStateCensus(fd)
	if err != nil {
		return ob, nil, err
	}
	ob.Version = 1
	if len(names) == 0 {
		if pendingScope != "" {
			return ob, nil, errS3OwnerPending
		}
		return ob, nil, nil
	}
	fileFD, err := unix.Openat(fd, s3OwnerStateName, unix.O_RDONLY|unix.O_NOFOLLOW|unix.O_CLOEXEC|unix.O_NONBLOCK, 0)
	if err != nil {
		return ob, nil, err
	}
	file = os.NewFile(uintptr(fileFD), "observed teardown state")
	defer func() {
		if resultErr != nil {
			_ = file.Close()
			file = nil
		}
	}()
	var st unix.Stat_t
	if !privateS3StateFile(file) || s3StateFilePolicy(fileFD, false) != nil || unix.Fstat(fileFD, &st) != nil {
		return ob, file, errS3OwnerInvalid
	}
	raw, err := io.ReadAll(io.LimitReader(file, maxS3OwnerBytes+1))
	defer zeroBytes(raw)
	if err != nil || len(raw) > maxS3OwnerBytes || uniqueS3StateJSON(raw) != nil || completeS3StateJSON(raw) != nil {
		return ob, file, errS3OwnerInvalid
	}
	var record s3OwnerRecord
	defer eraseTeardownRecord(&record)
	if json.Unmarshal(raw, &record) != nil || record.validate() != nil || record.Destination != destination || record.RecoveryHold != nil {
		return ob, file, errS3OwnerPending
	}
	ob.Present = true
	ob.Device = uint64(st.Dev)
	ob.Inode = uint64(st.Ino)
	if record.Active != nil {
		active := record.Active
		if pendingScope == "" || active.Operation != OpTeardown || active.TeardownScope != pendingScope {
			return ob, file, errS3OwnerPending
		}
		ob.ID = active.ID
		ob.Operation = active.Operation
		ob.Active = true
		ob.Scope = active.TeardownScope
		ob.Origin = active.TeardownOrigin
	} else {
		complete := record.Complete
		ob.ID = complete.ID
		ob.Operation = complete.Operation
		ob.Scope = complete.TeardownScope
		ob.Origin = complete.TeardownOrigin
		ob.Reconciled = complete.TeardownReconciled
		if pendingScope != "" && (ob.Operation != OpTeardown || ob.Scope != pendingScope) {
			return ob, file, errS3OwnerPending
		}
	}
	if !ob.valid() {
		return ob, file, errS3OwnerInvalid
	}
	if matches, err := pinnedEntryMatches(fd, s3OwnerStateName, file); err != nil || !matches {
		return ob, file, errS3OwnerInvalid
	}
	return ob, file, nil
}

func lockTeardownState(fd int, destination s3Destination, pendingScope string) (owned *os.File, ob s3TeardownObservation, err error) {
	if pendingScope != "" && !teardownHex(pendingScope, 64) {
		return nil, ob, errS3OwnerInvalid
	}
	if err = teardownDirectoryPolicy(fd); err != nil {
		return nil, ob, err
	}
	copyFD, err := unix.Openat(fd, ".", unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC, 0)
	if err != nil {
		return nil, ob, err
	}
	owned = os.NewFile(uintptr(copyFD), "locked teardown state")
	defer func() {
		if err != nil {
			_ = owned.Close()
			owned = nil
		}
	}()
	if err = unix.Flock(copyFD, unix.LOCK_EX|unix.LOCK_NB); err != nil {
		return owned, ob, errS3OwnerBusy
	}
	var state *os.File
	ob, state, err = observeTeardownState(copyFD, destination, pendingScope)
	if state != nil {
		err = errors.Join(err, state.Close())
	}
	return owned, ob, err
}

//export restic_bridge_s3_teardown_observe_v1
func restic_bridge_s3_teardown_observe_v1(stateFD C.int, destination *C.char, destinationLen C.uint32_t, pendingScope *C.char, pendingLen C.uint32_t, outFD *C.int, output *C.char, capacity C.uint32_t, outLen *C.uint32_t) C.int32_t {
	if outFD == nil || outLen == nil {
		return C.int32_t(StatusInvalidArgument)
	}
	*outFD = -1
	*outLen = 0
	if destination == nil || destinationLen == 0 || destinationLen > 4096 || output == nil || capacity < 4096 || capacity > 4096 || (pendingLen != 0 && (pendingLen != 64 || pendingScope == nil)) {
		return C.int32_t(StatusInvalidArgument)
	}
	var dest s3Destination
	if strictTeardownJSON(copyCBytes(destination, destinationLen), &dest) != nil {
		return C.int32_t(StatusInvalidArgument)
	}
	owned, ob, err := lockTeardownState(int(stateFD), dest, string(copyCBytes(pendingScope, pendingLen)))
	if err != nil {
		if errors.Is(err, errS3OwnerBusy) {
			return C.int32_t(StatusBusy)
		}
		return C.int32_t(StatusFailed)
	}
	raw, err := json.Marshal(ob)
	if err != nil || len(raw) > int(capacity) {
		_ = owned.Close()
		return C.int32_t(StatusFailed)
	}
	// The descriptor is intentionally transferred; Go must not retain a finalizer.
	fd := int(owned.Fd())
	unix.CloseOnExec(fd)
	// Dup would share the same open file description and lock. Detach with a
	// duplicate, then close only Go's wrapper after making the owned fd explicit.
	transferred, err := unix.FcntlInt(uintptr(fd), unix.F_DUPFD_CLOEXEC, 0)
	if err != nil {
		_ = owned.Close()
		return C.int32_t(StatusFailed)
	}
	if owned.Close() != nil {
		_ = unix.Close(transferred)
		return C.int32_t(StatusFailed)
	}
	copy(unsafe.Slice((*byte)(unsafe.Pointer(output)), len(raw)), raw)
	*outLen = C.uint32_t(len(raw))
	*outFD = C.int(transferred)
	return C.int32_t(StatusOk)
}

//export restic_bridge_s3_teardown_verify_v1
func restic_bridge_s3_teardown_verify_v1(stateFD C.int, destination *C.char, destinationLen C.uint32_t, expected *C.char, expectedLen C.uint32_t) C.int32_t {
	if destination == nil || destinationLen == 0 || destinationLen > 4096 || expected == nil || expectedLen == 0 || expectedLen > 4096 {
		return C.int32_t(StatusInvalidArgument)
	}
	var dest s3Destination
	var ob s3TeardownObservation
	if strictTeardownJSON(copyCBytes(destination, destinationLen), &dest) != nil || strictTeardownJSON(copyCBytes(expected, expectedLen), &ob) != nil || !ob.valid() || ob.Active || teardownDirectoryPolicy(int(stateFD)) != nil {
		return C.int32_t(StatusInvalidArgument)
	}
	if unix.Flock(int(stateFD), unix.LOCK_EX|unix.LOCK_NB) != nil {
		return C.int32_t(StatusBusy)
	}
	actual, file, err := observeTeardownState(int(stateFD), dest, ob.Scope)
	if file != nil {
		defer file.Close()
	}
	if err != nil || actual != ob {
		return C.int32_t(StatusFailed)
	}
	return C.int32_t(StatusOk)
}

//export restic_bridge_s3_teardown_retire_v1
func restic_bridge_s3_teardown_retire_v1(stateFD C.int, destination *C.char, destinationLen C.uint32_t, expected *C.char, expectedLen C.uint32_t) C.int32_t {
	if destination == nil || destinationLen == 0 || destinationLen > 4096 || expected == nil || expectedLen == 0 || expectedLen > 4096 {
		return C.int32_t(StatusInvalidArgument)
	}
	var dest s3Destination
	var ob s3TeardownObservation
	if strictTeardownJSON(copyCBytes(destination, destinationLen), &dest) != nil || strictTeardownJSON(copyCBytes(expected, expectedLen), &ob) != nil || !ob.valid() || ob.Active {
		return C.int32_t(StatusInvalidArgument)
	}
	return C.int32_t(retireObservedTeardownState(int(stateFD), dest, ob))
}

func retireObservedTeardownState(fd int, destination s3Destination, expected s3TeardownObservation) int32 {
	// The caller's transferred descriptor retains this exclusion until Rust Drop.
	if !expected.valid() || expected.Active || (expected.Operation == OpTeardown && !expected.Reconciled) || teardownDirectoryPolicy(fd) != nil {
		return StatusInvalidArgument
	}
	if unix.Flock(fd, unix.LOCK_EX|unix.LOCK_NB) != nil {
		return StatusBusy
	}
	actual, file, err := observeTeardownState(fd, destination, expected.Scope)
	if file != nil {
		defer file.Close()
	}
	if err != nil || actual != expected {
		return StatusFailed
	}
	if !expected.Present {
		return StatusOk
	}
	var entropy [16]byte
	if _, err := rand.Read(entropy[:]); err != nil {
		return StatusFailed
	}
	claim := ".teardown-retire-" + hex.EncodeToString(entropy[:])
	if err := publishDirectory(fd, s3OwnerStateName, fd, claim); err != nil {
		return StatusFailed
	}
	// Never unlink a same-name replacement or claim whose original identity is lost.
	if matches, err := pinnedEntryMatches(fd, claim, file); err != nil || !matches {
		return StatusFailed
	}
	if err := unix.Unlinkat(fd, claim, 0); err != nil {
		return StatusFailed
	}
	if syncDirectory(fd) != nil {
		return StatusFailed
	}
	names, err := teardownStateCensus(fd)
	if err != nil || len(names) != 0 {
		return StatusFailed
	}
	return StatusOk
}
