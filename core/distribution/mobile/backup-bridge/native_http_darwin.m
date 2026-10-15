//go:build darwin || ios

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#import <Foundation/Foundation.h>
#import <Security/SecProtocolOptions.h>
#include "native_http_internal.h"

static const NSUInteger SBHeaderLimit = 32 * 1024;
static const NSUInteger SBMetadataLimit = 256 * 1024;
static const NSUInteger SBObjectLimit = 64 * 1024 * 1024;
static const NSUInteger SBErrorLimit = 64 * 1024;

@interface SBHTTPCall : NSObject <NSURLSessionDataDelegate, NSURLSessionTaskDelegate> {
    NSCondition *_condition;
    NSOperationQueue *_queue;
    NSURLSession *_session;
    NSURLSessionDataTask *_task;
    NSMutableData *_requestBody;
    NSMutableData *_responseBody;
    NSDictionary *_responseHeaders;
    NSData *_metadata;
    NSUInteger _limit;
    NSInteger _status;
    int32_t _code;
    BOOL _head;
    BOOL _started;
    BOOL _completed;
    BOOL _invalidated;
    BOOL _joined;
    BOOL _responseSeen;
}
- (id)initWithMetadata:(NSDictionary *)metadata body:(NSData *)body limit:(NSUInteger)limit;
- (void)start;
- (void)cancel;
- (int32_t)waitAndJoin;
- (const void *)metadataBytes:(uint32_t *)length;
- (const void *)bodyBytes:(uint32_t *)length;
@end

@implementation SBHTTPCall
- (id)initWithMetadata:(NSDictionary *)metadata body:(NSData *)body limit:(NSUInteger)limit {
    self = [super init];
    if (!self) return nil;
    NSString *urlText = metadata[@"url"];
    NSString *method = metadata[@"method"];
    NSDictionary *headers = metadata[@"headers"];
    if (![urlText isKindOfClass:[NSString class]] || ![method isKindOfClass:[NSString class]] ||
        ![headers isKindOfClass:[NSDictionary class]] || headers.count > 256 ||
        ![@[@"GET", @"HEAD", @"PUT", @"DELETE"] containsObject:method]) {
        [self release];
        return nil;
    }
    NSURL *url = [NSURL URLWithString:urlText];
    if (!url || ![url.scheme isEqualToString:@"https"] || !url.host.length || url.user || url.password || url.fragment ||
        ![url.absoluteString isEqualToString:urlText]) {
        [self release];
        return nil;
    }
    _head = [method isEqualToString:@"HEAD"];
    if (![method isEqualToString:@"PUT"] && body.length != 0) {
        [self release];
        return nil;
    }
    NSMutableURLRequest *request = [NSMutableURLRequest requestWithURL:url
        cachePolicy:NSURLRequestReloadIgnoringLocalCacheData timeoutInterval:30];
    request.HTTPMethod = method;
    request.HTTPShouldHandleCookies = NO;
    NSUInteger headerBytes = 0;
    for (id key in headers) {
        id values = headers[key];
        if (![key isKindOfClass:[NSString class]] || ![values isKindOfClass:[NSArray class]] ||
            [values count] == 0 || [values count] > 256) {
            [self release];
            return nil;
        }
        headerBytes += [key lengthOfBytesUsingEncoding:NSUTF8StringEncoding];
        for (id value in values) {
            if (![value isKindOfClass:[NSString class]]) { [self release]; return nil; }
            NSUInteger size = [value lengthOfBytesUsingEncoding:NSUTF8StringEncoding];
            if (headerBytes > SBHeaderLimit || size > SBHeaderLimit - headerBytes) { [self release]; return nil; }
            headerBytes += size;
        }
        [request setValue:[values componentsJoinedByString:@","] forHTTPHeaderField:key];
    }
    _condition = [[NSCondition alloc] init];
    _queue = [[NSOperationQueue alloc] init];
    _queue.maxConcurrentOperationCount = 1;
    _queue.name = @"solstone.backup.http";
    _requestBody = [body mutableCopy];
    _responseBody = [[NSMutableData alloc] init];
    _limit = limit;
    if (body.length != 0) request.HTTPBody = _requestBody;
    NSURLSessionConfiguration *configuration = [NSURLSessionConfiguration ephemeralSessionConfiguration];
    configuration.URLCredentialStorage = nil;
    configuration.HTTPCookieStorage = nil;
    configuration.URLCache = nil;
    configuration.HTTPShouldSetCookies = NO;
    configuration.HTTPCookieAcceptPolicy = NSHTTPCookieAcceptPolicyNever;
    configuration.connectionProxyDictionary = @{};
    configuration.requestCachePolicy = NSURLRequestReloadIgnoringLocalCacheData;
    configuration.timeoutIntervalForRequest = 30;
    configuration.timeoutIntervalForResource = 600;
    configuration.TLSMinimumSupportedProtocolVersion = tls_protocol_version_TLSv12;
    _session = [[NSURLSession sessionWithConfiguration:configuration delegate:self delegateQueue:_queue] retain];
    _task = [[_session dataTaskWithRequest:request] retain];
    return self;
}

- (void)start {
    [_condition lock];
    if (!_started) {
        _started = YES;
        if (_code == 3) [_task cancel];
        else [_task resume];
    }
    [_condition unlock];
}

- (void)cancel {
    [_condition lock];
    // Completion and cancellation linearize here. Invalidation/queue joining
    // still follow; a later cancel cannot rewrite the completed HTTP result.
    if (!_completed && !_invalidated) {
        if (_code == 0) _code = 3;
        [_task cancel];
    }
    [_condition unlock];
}

- (void)URLSession:(NSURLSession *)session task:(NSURLSessionTask *)task
    willPerformHTTPRedirection:(NSHTTPURLResponse *)response newRequest:(NSURLRequest *)request
    completionHandler:(void (^)(NSURLRequest *))completionHandler {
    (void)session; (void)task; (void)response; (void)request;
    [_condition lock];
    if (_code == 0) _code = 4;
    [_condition unlock];
    completionHandler(nil);
}

- (void)handleChallenge:(NSURLAuthenticationChallenge *)challenge
    completionHandler:(void (^)(NSURLSessionAuthChallengeDisposition, NSURLCredential *))completionHandler {
    if ([challenge.protectionSpace.authenticationMethod isEqualToString:NSURLAuthenticationMethodServerTrust]) {
        completionHandler(NSURLSessionAuthChallengePerformDefaultHandling, nil);
    } else {
        [_condition lock];
        if (_code == 0) _code = 5;
        [_condition unlock];
        completionHandler(NSURLSessionAuthChallengeCancelAuthenticationChallenge, nil);
    }
}

- (void)URLSession:(NSURLSession *)session didReceiveChallenge:(NSURLAuthenticationChallenge *)challenge
    completionHandler:(void (^)(NSURLSessionAuthChallengeDisposition, NSURLCredential *))completionHandler {
    (void)session;
    [self handleChallenge:challenge completionHandler:completionHandler];
}

- (void)URLSession:(NSURLSession *)session task:(NSURLSessionTask *)task
    didReceiveChallenge:(NSURLAuthenticationChallenge *)challenge
    completionHandler:(void (^)(NSURLSessionAuthChallengeDisposition, NSURLCredential *))completionHandler {
    (void)session; (void)task;
    [self handleChallenge:challenge completionHandler:completionHandler];
}

- (void)URLSession:(NSURLSession *)session dataTask:(NSURLSessionDataTask *)task
    didReceiveResponse:(NSURLResponse *)response
    completionHandler:(void (^)(NSURLSessionResponseDisposition))completionHandler {
    (void)session; (void)task;
    [_condition lock];
    if (_responseSeen || ![response isKindOfClass:[NSHTTPURLResponse class]]) {
        if (_code == 0) _code = 8;
    } else {
        _responseSeen = YES;
        NSHTTPURLResponse *http = (NSHTTPURLResponse *)response;
        _status = http.statusCode;
        if ((_status < 200 || _status >= 300) && _limit > SBErrorLimit) _limit = SBErrorLimit;
        NSMutableDictionary *headers = [NSMutableDictionary dictionary];
        NSUInteger headerBytes = 0;
        for (id key in http.allHeaderFields) {
            id value = http.allHeaderFields[key];
            if (![key isKindOfClass:[NSString class]] || ![value isKindOfClass:[NSString class]]) {
                if (_code == 0) _code = 8;
                break;
            }
            NSUInteger nameSize = [key lengthOfBytesUsingEncoding:NSUTF8StringEncoding];
            NSUInteger valueSize = [value lengthOfBytesUsingEncoding:NSUTF8StringEncoding];
            if (nameSize > SBHeaderLimit - headerBytes || valueSize > SBHeaderLimit - headerBytes - nameSize) {
                if (_code == 0) _code = 2;
                break;
            }
            headerBytes += nameSize + valueSize;
            NSString *name = [key lowercaseString];
            NSString *lower = [value lowercaseString];
            if (([name isEqualToString:@"content-encoding"] && ![lower isEqualToString:@"identity"]) ||
                ([name isEqualToString:@"content-type"] && [lower hasPrefix:@"multipart/"])) {
                if (_code == 0) _code = 8;
                break;
            }
            headers[key] = @[value];
        }
        if (http.allHeaderFields.count > 256) { if (_code == 0) _code = 2; }
        if (!_head && response.expectedContentLength > 0 && (uint64_t)response.expectedContentLength > _limit) {
            if (_code == 0) _code = 2;
        }
        _responseHeaders = [headers copy];
    }
    BOOL allowed = _code == 0;
    [_condition unlock];
    completionHandler(allowed ? NSURLSessionResponseAllow : NSURLSessionResponseCancel);
}

- (void)URLSession:(NSURLSession *)session dataTask:(NSURLSessionDataTask *)task didReceiveData:(NSData *)data {
    (void)session;
    [_condition lock];
    if (_code == 0) {
        if (!_responseSeen || _head) _code = 8;
        else if (data.length > _limit - _responseBody.length) _code = 2;
        else [_responseBody appendData:data];
    }
    BOOL cancel = _code != 0;
    [_condition unlock];
    if (cancel) [task cancel];
}

- (void)URLSession:(NSURLSession *)session dataTask:(NSURLSessionDataTask *)task
    willCacheResponse:(NSCachedURLResponse *)response completionHandler:(void (^)(NSCachedURLResponse *))completionHandler {
    (void)session; (void)task; (void)response;
    completionHandler(nil);
}

- (void)URLSession:(NSURLSession *)session task:(NSURLSessionTask *)task didCompleteWithError:(NSError *)error {
    (void)task;
    [_condition lock];
    if (_code == 0 && error) {
        if ([error.domain isEqualToString:NSURLErrorDomain]) {
            switch (error.code) {
                case NSURLErrorCancelled: _code = 3; break;
                case NSURLErrorSecureConnectionFailed:
                case NSURLErrorServerCertificateHasBadDate:
                case NSURLErrorServerCertificateUntrusted:
                case NSURLErrorServerCertificateHasUnknownRoot:
                case NSURLErrorServerCertificateNotYetValid:
                case NSURLErrorClientCertificateRejected:
                case NSURLErrorClientCertificateRequired: _code = 9; break;
                default: _code = 6; break;
            }
        } else _code = 6;
    }
    if (_code == 0 && (!_responseSeen || _status < 100 || _status > 599)) _code = 8;
    _completed = YES;
    [_condition unlock];
    [session finishTasksAndInvalidate];
}

- (void)URLSession:(NSURLSession *)session didBecomeInvalidWithError:(NSError *)error {
    (void)session;
    [_condition lock];
    if (_code == 0 && error) _code = 6;
    if (_code == 0 && !_completed) _code = 8;
    _invalidated = YES;
    [_condition broadcast];
    [_condition unlock];
}

- (int32_t)waitAndJoin {
    [_condition lock];
    while (!_invalidated) [_condition wait];
    [_condition unlock];
    // This must run outside the serial delegate queue. Seeing the final
    // callback begin is insufficient; join its actual operation too.
    [_queue waitUntilAllOperationsAreFinished];
    [_condition lock];
    if (!_completed || !_invalidated || _queue.operationCount != 0) { if (_code == 0) _code = 8; }
    _joined = YES;
    if (_code == 0 && !_metadata) {
        _metadata = [[NSJSONSerialization dataWithJSONObject:@{@"status":@(_status), @"headers":_responseHeaders ?: @{}}
            options:0 error:NULL] retain];
        if (!_metadata || _metadata.length > SBMetadataLimit) _code = 2;
    }
    int32_t code = _code;
    [_condition unlock];
    return code;
}

- (const void *)metadataBytes:(uint32_t *)length {
    if (!_joined || _code != 0 || !length) return NULL;
    *length = (uint32_t)_metadata.length;
    return _metadata.bytes;
}

- (const void *)bodyBytes:(uint32_t *)length {
    if (!_joined || _code != 0 || !length) return NULL;
    *length = (uint32_t)_responseBody.length;
    return _responseBody.bytes;
}

- (void)dealloc {
    [_task release];
    [_session release];
    [_queue release];
    [_condition release];
    if (_requestBody.length) memset(_requestBody.mutableBytes, 0, _requestBody.length);
    if (_responseBody.length) memset(_responseBody.mutableBytes, 0, _responseBody.length);
    [_requestBody release];
    [_responseBody release];
    [_responseHeaders release];
    [_metadata release];
    [super dealloc];
}
@end

sb_http_call *sb_http_create(const void *metadata, uint32_t metadata_len, const void *body,
                           uint32_t body_len, uint32_t response_limit) {
    @autoreleasepool {
        if (!metadata || !metadata_len || metadata_len > SBHeaderLimit || body_len > SBObjectLimit ||
            (body_len && !body) || !response_limit || response_limit > SBObjectLimit) return NULL;
        NSData *encoded = [NSData dataWithBytes:metadata length:metadata_len];
        id decoded = [NSJSONSerialization JSONObjectWithData:encoded options:0 error:NULL];
        if (![decoded isKindOfClass:[NSDictionary class]]) return NULL;
        NSData *payload = [NSData dataWithBytes:body length:body_len];
        return (sb_http_call *)[[SBHTTPCall alloc] initWithMetadata:decoded body:payload limit:response_limit];
    }
}

void sb_http_start(sb_http_call *call) { @autoreleasepool { [(SBHTTPCall *)call start]; } }
void sb_http_cancel(sb_http_call *call) { @autoreleasepool { [(SBHTTPCall *)call cancel]; } }
int32_t sb_http_wait(sb_http_call *call) { @autoreleasepool { return call ? [(SBHTTPCall *)call waitAndJoin] : 1; } }
const void *sb_http_metadata(sb_http_call *call, uint32_t *length) {
    if (length) *length = 0;
    return call ? [(SBHTTPCall *)call metadataBytes:length] : NULL;
}
const void *sb_http_body(sb_http_call *call, uint32_t *length) {
    if (length) *length = 0;
    return call ? [(SBHTTPCall *)call bodyBytes:length] : NULL;
}
void sb_http_release(sb_http_call *call) { @autoreleasepool { [(SBHTTPCall *)call release]; } }
