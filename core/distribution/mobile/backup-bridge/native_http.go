// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

package main

/*
#include <stdint.h>
*/
import "C"

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/netip"
	"net/url"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"
	"unsafe"
)

const (
	maxHTTPURLBytes      = 8192
	maxHTTPHeaderBytes   = 32 * 1024
	maxHTTPMetadataBytes = 256 * 1024
	maxHTTPObjectBytes   = 64 * 1024 * 1024
	maxHTTPListBytes     = 1024 * 1024
	maxHTTPErrorBytes    = 64 * 1024
)

var (
	errHTTPArgument    = errors.New("HTTP_REQUEST_INVALID")
	errHTTPAuthority   = errors.New("HTTP_AUTHORITY_REFUSED")
	errHTTPLimit       = errors.New("HTTP_LIMIT_EXCEEDED")
	errHTTPBody        = errors.New("HTTP_BODY_SOURCE_FAILED")
	errHTTPRedirect    = errors.New("HTTP_REDIRECT_REFUSED")
	errHTTPAuth        = errors.New("HTTP_AUTHENTICATION_REFUSED")
	errHTTPNetwork     = errors.New("HTTP_NETWORK_FAILED")
	errHTTPUnsupported = errors.New("HTTP_PLATFORM_UNAVAILABLE")
	errHTTPProtocol    = errors.New("HTTP_RESPONSE_INVALID")
	errHTTPTLS         = errors.New("HTTP_TLS_FAILED")
)

// nativeHTTPTransport is private to the callable archive. It delegates every
// network request to the fixed Apple implementation; it never creates a Go
// HTTP transport, resolver, dialer, credential provider or replay worker.
type nativeHTTPTransport struct {
	ctx       context.Context
	authority string
}

type nativeHTTPRequest struct {
	URL     string              `json:"url"`
	Method  string              `json:"method"`
	Headers map[string][]string `json:"headers"`
}

type nativeHTTPReply struct {
	Status  int                 `json:"status"`
	Headers map[string][]string `json:"headers"`
	Body    []byte              `json:"-"`
}

func httpAuthority(value *url.URL) (string, error) {
	if value == nil || value.Scheme != "https" || value.Host == "" || value.User != nil || value.Opaque != "" || value.Fragment != "" {
		return "", errHTTPAuthority
	}
	parsed, parseErr := url.Parse("https://" + value.Host)
	if parseErr != nil || parsed.Host != value.Host || parsed.User != nil || parsed.Path != "" || parsed.RawQuery != "" || parsed.Fragment != "" {
		return "", errHTTPAuthority
	}
	host := value.Hostname()
	if host == "" || len(host) > 253 {
		return "", errHTTPAuthority
	}
	if _, err := netip.ParseAddr(host); err != nil {
		for _, ch := range host {
			if !(ch >= 'a' && ch <= 'z' || ch >= 'A' && ch <= 'Z' || ch >= '0' && ch <= '9' || ch == '-' || ch == '.') {
				return "", errHTTPAuthority
			}
		}
	}
	port := value.Port()
	if port == "" {
		port = "443"
	}
	n, err := strconv.ParseUint(port, 10, 16)
	if err != nil || n == 0 {
		return "", errHTTPAuthority
	}
	return strings.ToLower(host) + ":" + strconv.FormatUint(n, 10), nil
}

func newNativeHTTPTransport(ctx context.Context, endpoint string) (*nativeHTTPTransport, error) {
	if ctx == nil || len(endpoint) > maxHTTPURLBytes {
		return nil, errHTTPArgument
	}
	parsed, err := url.Parse(endpoint)
	if err != nil || parsed.Path != "" && parsed.Path != "/" || parsed.RawQuery != "" || parsed.ForceQuery {
		return nil, errHTTPAuthority
	}
	authority, err := httpAuthority(parsed)
	if err != nil {
		return nil, err
	}
	return &nativeHTTPTransport{ctx: ctx, authority: authority}, nil
}

func validHTTPHeader(name string, values []string) bool {
	if name == "" || len(values) == 0 {
		return false
	}
	for _, ch := range name {
		if !(ch >= 'a' && ch <= 'z' || ch >= 'A' && ch <= 'Z' || ch >= '0' && ch <= '9' || strings.ContainsRune("!#$%&'*+-.^_`|~", ch)) {
			return false
		}
	}
	for _, value := range values {
		for _, ch := range value {
			if ch > 126 || ch < 32 && ch != '\t' {
				return false
			}
		}
	}
	return true
}

// prepareNativeHTTPRequest owns and closes Body even when validation refuses.
// Blocking local reads/Close remain on the owning worker, never detached.
func (transport *nativeHTTPTransport) prepareNativeHTTPRequest(request *http.Request) (wire nativeHTTPRequest, body []byte, err error) {
	if request == nil {
		return wire, nil, errHTTPArgument
	}
	if request.Body != nil {
		defer func() {
			closeErr := request.Body.Close()
			if closeErr != nil && err == nil {
				err = errHTTPBody
			}
			if err != nil {
				zeroBytes(body)
				body = nil
			}
		}()
	}
	authority, authErr := httpAuthority(request.URL)
	if authErr != nil || authority != transport.authority {
		return wire, nil, errHTTPAuthority
	}
	if request.Host != "" && request.Host != request.URL.Host {
		// Host participates in SigV4 bytes. Do not admit an equivalent spelling
		// and then silently replace it with the URL spelling in Foundation.
		return wire, nil, errHTTPAuthority
	}
	if request.RequestURI != "" || request.URL.ForceQuery || request.Trailer != nil || len(request.TransferEncoding) != 0 {
		return wire, nil, errHTTPArgument
	}
	if request.URL.RawPath != "" && request.URL.RawPath != request.URL.EscapedPath() {
		return wire, nil, errHTTPArgument
	}
	switch request.Method {
	case http.MethodGet, http.MethodHead, http.MethodDelete:
		if request.ContentLength != 0 || request.Body != nil && request.Body != http.NoBody {
			return wire, nil, errHTTPArgument
		}
	case http.MethodPut, http.MethodPost:
	default:
		return wire, nil, errHTTPArgument
	}
	if request.ContentLength < 0 {
		return wire, nil, errHTTPArgument
	}
	if request.ContentLength > maxHTTPObjectBytes || len(request.Header) > 256 {
		return wire, nil, errHTTPLimit
	}
	if len(request.URL.Path) > maxHTTPURLBytes || len(request.URL.RawPath) > maxHTTPURLBytes || len(request.URL.RawQuery) > maxHTTPURLBytes {
		return wire, nil, errHTTPLimit
	}
	wire.URL = request.URL.String()
	if len(wire.URL) > maxHTTPURLBytes {
		return wire, nil, errHTTPLimit
	}
	if _, err := url.ParseQuery(request.URL.RawQuery); err != nil {
		return wire, nil, errHTTPArgument
	}
	wire.Method = request.Method
	wire.Headers = make(map[string][]string, len(request.Header))
	seenNames := make(map[string]bool, len(request.Header))
	headerBytes := 0
	headerValues := 0
	for name, values := range request.Header {
		headerValues += len(values)
		if len(name) > maxHTTPHeaderBytes || headerValues > 256 {
			return wire, nil, errHTTPLimit
		}
		if !validHTTPHeader(name, values) {
			return wire, nil, errHTTPArgument
		}
		lowerName := strings.ToLower(name)
		if seenNames[lowerName] {
			return wire, nil, errHTTPArgument
		}
		seenNames[lowerName] = true
		switch lowerName {
		case "cookie", "cookie2", "proxy-authorization", "proxy-connection", "trailer", "transfer-encoding":
			return wire, nil, errHTTPArgument
		case "host":
			if len(values) != 1 || values[0] != request.URL.Host {
				return wire, nil, errHTTPAuthority
			}
		case "content-length":
			if len(values) != 1 || values[0] != strconv.FormatInt(request.ContentLength, 10) {
				return wire, nil, errHTTPArgument
			}
		}
		headerBytes += len(name)
		for _, value := range values {
			if len(value) > maxHTTPHeaderBytes-headerBytes {
				return wire, nil, errHTTPLimit
			}
			headerBytes += len(value)
		}
		if headerBytes > maxHTTPHeaderBytes {
			return wire, nil, errHTTPLimit
		}
		wire.Headers[name] = append([]string(nil), values...)
	}
	if request.Body == nil || request.Body == http.NoBody {
		if request.ContentLength != 0 {
			return wire, nil, errHTTPArgument
		}
		return wire, nil, nil
	}
	body, readErr := io.ReadAll(io.LimitReader(request.Body, request.ContentLength+1))
	if readErr != nil {
		return wire, body, errHTTPBody
	}
	if int64(len(body)) != request.ContentLength {
		return wire, body, errHTTPBody
	}
	return wire, body, nil
}

func (transport *nativeHTTPTransport) RoundTrip(request *http.Request) (*http.Response, error) {
	wire, body, err := transport.prepareNativeHTTPRequest(request)
	if err != nil {
		return nil, err
	}
	defer zeroBytes(body)
	encoded, err := json.Marshal(wire)
	if err != nil {
		return nil, errHTTPArgument
	}
	defer zeroBytes(encoded)
	if len(encoded) > maxHTTPHeaderBytes {
		return nil, errHTTPLimit
	}
	ctx, cancel := context.WithCancel(transport.ctx)
	requestCancelled := make(chan struct{})
	stopRequestCancellation := context.AfterFunc(request.Context(), func() {
		cancel()
		close(requestCancelled)
	})
	defer func() {
		if !stopRequestCancellation() {
			<-requestCancelled
		}
		cancel()
	}()
	if ctx.Err() != nil {
		return nil, context.Canceled
	}
	limit := maxHTTPObjectBytes
	if request.URL.Query().Get("list-type") == "2" {
		limit = maxHTTPListBytes
	}
	reply, err := performNativeHTTP(ctx, encoded, body, limit)
	if err != nil {
		return nil, err
	}
	if reply.Status < 100 || reply.Status > 599 || len(reply.Headers) > 256 || len(reply.Body) > limit {
		zeroBytes(reply.Body)
		return nil, errHTTPProtocol
	}
	headers := make(http.Header, len(reply.Headers))
	headerBytes := 0
	for name, values := range reply.Headers {
		if !validHTTPHeader(name, values) {
			zeroBytes(reply.Body)
			return nil, errHTTPProtocol
		}
		headerBytes += len(name)
		for _, value := range values {
			headerBytes += len(value)
		}
		if headerBytes > maxHTTPHeaderBytes {
			zeroBytes(reply.Body)
			return nil, errHTTPLimit
		}
		for _, value := range values {
			headers.Add(name, value)
		}
	}
	length := int64(len(reply.Body))
	if value := headers.Get("Content-Length"); value != "" {
		length, err = strconv.ParseInt(value, 10, 64)
		if err != nil || length < 0 || request.Method != http.MethodHead && length != int64(len(reply.Body)) {
			zeroBytes(reply.Body)
			return nil, errHTTPProtocol
		}
	}
	return &http.Response{
		StatusCode: reply.Status,
		Status:     strconv.Itoa(reply.Status) + " " + http.StatusText(reply.Status),
		Header:     headers, ContentLength: length, Request: request,
		Body: &nativeHTTPResponseBody{reader: bytes.NewReader(reply.Body), owned: reply.Body},
	}, nil
}

type nativeHTTPResponseBody struct {
	mu     sync.Mutex
	reader *bytes.Reader
	owned  []byte
}

func (body *nativeHTTPResponseBody) Read(buffer []byte) (int, error) {
	body.mu.Lock()
	defer body.mu.Unlock()
	if body.reader == nil {
		return 0, io.ErrClosedPipe
	}
	return body.reader.Read(buffer)
}

func (body *nativeHTTPResponseBody) Close() error {
	body.mu.Lock()
	defer body.mu.Unlock()
	zeroBytes(body.owned)
	body.owned = nil
	body.reader = nil
	return nil
}

func exchangeNativeHTTP(ctx context.Context, request *http.Request) (nativeHTTPReply, error) {
	if request == nil || request.URL == nil {
		return nativeHTTPReply{}, errHTTPArgument
	}
	origin := "https://" + request.URL.Host
	transport, err := newNativeHTTPTransport(ctx, origin)
	if err != nil {
		return nativeHTTPReply{}, err
	}
	wire, body, err := transport.prepareNativeHTTPRequest(request)
	if err != nil {
		return nativeHTTPReply{}, err
	}
	defer zeroBytes(body)
	encoded, err := json.Marshal(wire)
	if err != nil {
		return nativeHTTPReply{}, errHTTPArgument
	}
	defer zeroBytes(encoded)
	if len(encoded) > maxHTTPHeaderBytes {
		return nativeHTTPReply{}, errHTTPLimit
	}
	limit := maxHTTPObjectBytes
	if request.URL.Query().Get("list-type") == "2" {
		limit = maxHTTPListBytes
	}
	reply, err := performNativeHTTP(ctx, encoded, body, limit)
	if err != nil {
		return nativeHTTPReply{}, err
	}
	if reply.Status < 100 || reply.Status > 599 || len(reply.Headers) > 256 || len(reply.Body) > limit {
		zeroBytes(reply.Body)
		return nativeHTTPReply{}, errHTTPProtocol
	}
	for name, values := range reply.Headers {
		if !validHTTPHeader(name, values) {
			zeroBytes(reply.Body)
			return nativeHTTPReply{}, errHTTPProtocol
		}
	}
	return reply, nil
}

func nativeHTTPStatusCode(err error) int32 {
	if err == nil {
		return 0
	}
	if errors.Is(err, errHTTPArgument) || errors.Is(err, errHTTPAuthority) || errors.Is(err, errHTTPBody) {
		return 1
	}
	if errors.Is(err, errHTTPLimit) {
		return 2
	}
	if errors.Is(err, context.Canceled) {
		return 3
	}
	if errors.Is(err, errHTTPRedirect) {
		return 4
	}
	if errors.Is(err, errHTTPAuth) {
		return 5
	}
	if errors.Is(err, errHTTPUnsupported) {
		return 6
	}
	if errors.Is(err, errHTTPProtocol) {
		return 8
	}
	if errors.Is(err, errHTTPTLS) {
		return 9
	}
	return 7
}

//export restic_bridge_http_exchange
func restic_bridge_http_exchange(
	method *C.char, methodLen C.uint32_t,
	urlStr *C.char, urlLen C.uint32_t,
	headerJSON *C.char, headerJSONLen C.uint32_t,
	body *C.char, bodyLen C.uint32_t,
	cancelled *C.uint32_t,
	outHTTPStatus *C.int32_t,
	outBody *C.char, outBodyCap C.uint32_t, outBodyLen *C.uint32_t,
) C.int32_t {
	if outHTTPStatus != nil {
		*outHTTPStatus = 0
	}
	if outBodyLen != nil {
		*outBodyLen = 0
	}
	if cancelled != nil && atomic.LoadUint32((*uint32)(unsafe.Pointer(cancelled))) != 0 {
		return 3
	}
	m := readString(method, methodLen)
	u := readString(urlStr, urlLen)
	headersRaw := readBytes(headerJSON, headerJSONLen)
	bodyBytes := readBytes(body, bodyLen)
	defer zeroBytes(bodyBytes)

	if len(m) == 0 || len(u) == 0 {
		return 1
	}
	var headers map[string][]string
	if len(headersRaw) != 0 {
		if err := json.Unmarshal(headersRaw, &headers); err != nil {
			return 1
		}
	}
	parsedURL, err := url.Parse(u)
	if err != nil {
		return 1
	}
	req := &http.Request{
		Method: m,
		URL:    parsedURL,
		Header: make(http.Header),
	}
	for k, vs := range headers {
		for _, v := range vs {
			req.Header.Add(k, v)
		}
	}
	if len(bodyBytes) > 0 {
		req.Body = io.NopCloser(bytes.NewReader(bodyBytes))
		req.ContentLength = int64(len(bodyBytes))
	} else {
		req.ContentLength = 0
		req.Body = http.NoBody
	}
	if clStr := req.Header.Get("Content-Length"); clStr != "" {
		if cl, err := strconv.ParseInt(clStr, 10, 64); err == nil {
			req.ContentLength = cl
		}
	}

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	if cancelled != nil {
		stopPolling := make(chan struct{})
		defer close(stopPolling)
		go func() {
			ticker := time.NewTicker(10 * time.Millisecond)
			defer ticker.Stop()
			for {
				select {
				case <-ctx.Done():
					return
				case <-stopPolling:
					return
				case <-ticker.C:
					if atomic.LoadUint32((*uint32)(unsafe.Pointer(cancelled))) != 0 {
						cancel()
						return
					}
				}
			}
		}()
	}

	reply, err := exchangeNativeHTTP(ctx, req)
	if err != nil {
		return C.int32_t(nativeHTTPStatusCode(err))
	}
	defer zeroBytes(reply.Body)

	if outHTTPStatus != nil {
		*outHTTPStatus = C.int32_t(reply.Status)
	}
	if outBodyLen != nil {
		*outBodyLen = C.uint32_t(len(reply.Body))
	}
	if len(reply.Body) > int(outBodyCap) {
		return 2
	}
	if outBody != nil && len(reply.Body) > 0 {
		copy(unsafe.Slice((*byte)(unsafe.Pointer(outBody)), outBodyCap), reply.Body)
	}
	return 0
}
