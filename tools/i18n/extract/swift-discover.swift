// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
import Foundation
import SwiftParser
import SwiftSyntax

struct Literal: Encodable {
    let start: Int
    let end: Int
    let raw: String
    let kind: String
    let context: String
}
final class Discovery: SyntaxVisitor {
    var literals: [Literal] = []
    let bytes: [UInt8]
    init(source: String) { bytes = Array(source.utf8); super.init(viewMode: .sourceAccurate) }
    override func visit(_ node: StringLiteralExprSyntax) -> SyntaxVisitorContinueKind {
        let start = node.positionAfterSkippingLeadingTrivia.utf8Offset
        let end = node.endPositionBeforeTrailingTrivia.utf8Offset
        let ancestors = sequence(first: node.parent, next: { $0?.parent }).compactMap { $0 }
        let macro = ancestors.first { $0.is(MacroExpansionExprSyntax.self) || $0.is(MacroExpansionDeclSyntax.self) }
        literals.append(Literal(start: start, end: end, raw: String(decoding: bytes[start..<end], as: UTF8.self), kind: "string", context: macro == nil ? "expression" : "macro"))
        // Interpolations remain whole editable expressions. Nested string tokens are inside this span.
        return .skipChildren
    }
}
func discover(_ source: String) throws -> [Literal] {
    let syntax = Parser.parse(source: source)
    guard !syntax.hasError else { throw NSError(domain: "Swift source contains parser errors", code: 1) }
    let discovery = Discovery(source: source)
    discovery.walk(syntax)
    return discovery.literals.sorted { $0.start < $1.start }
}

do {
    guard CommandLine.arguments.count == 2 else { throw NSError(domain: "usage: swift-discover SOURCE.swift | --self-test", code: 1) }
    if CommandLine.arguments[1] == "--self-test" {
        let source = #"let x = "中文İ"; #someMacro("label"); let y = "count \(10000)""#
        let literals = try discover(source)
        guard literals.count == 3, literals[0].raw == "\"中文İ\"", literals[1].context == "macro",
              literals[2].raw.contains("10000") else { throw NSError(domain: "literal/macro discovery control failed", code: 1) }
        let bytes = Array(source.utf8)
        for literal in literals {
            guard String(decoding: bytes[literal.start..<literal.end], as: UTF8.self) == literal.raw else {
                throw NSError(domain: "UTF-8 span control failed", code: 1)
            }
        }
        var refused = false
        do { _ = try discover("let broken = \"") } catch { refused = true }
        guard refused else { throw NSError(domain: "malformed-source control passed unexpectedly", code: 1) }
        print("SwiftSyntax UTF-8, macro, interpolation and refusal controls passed")
        exit(0)
    }
    let source = try String(contentsOfFile: CommandLine.arguments[1], encoding: .utf8)
    let literals = try discover(source)
    let encoder = JSONEncoder()
    encoder.outputFormatting = [.sortedKeys]
    FileHandle.standardOutput.write(try encoder.encode(literals))
    FileHandle.standardOutput.write(Data([10]))
} catch {
    FileHandle.standardError.write(Data("\(error)\n".utf8))
    exit(1)
}
