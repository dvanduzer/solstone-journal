# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc
import datetime,json,pathlib,subprocess,sys
root=pathlib.Path(__file__).parent
sdk=subprocess.check_output(['xcrun','--sdk','iphoneos','--show-sdk-path'],text=True).strip()
argv=['xcrun','swiftc','-typecheck','-parse-as-library','-target','arm64-apple-ios26.0','-sdk',sdk,'-import-objc-header',str(root/'Shared/Bridge.h'),str(root/'Host/Host.swift')]
r=subprocess.run(argv,cwd=root,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
(root/'swift-syntax.json').write_text(json.dumps({'time':datetime.datetime.now(datetime.timezone.utc).isoformat(),'argv':argv,'exit':r.returncode,'output':r.stdout,'boundary':'Harness Swift syntax only. No native archive or runtime proof.'},indent=2)+'\n')
print(r.stdout);print('SWIFT_TYPECHECK_EXIT',r.returncode)
if r.returncode:sys.exit(r.returncode)

objc=['xcrun','clang','-fsyntax-only','-fobjc-arc','-Wall','-Wextra','-Werror','-target','arm64-apple-ios26.0','-isysroot',sdk,'-I',str(root/'SyntaxHeaders'),str(root/'Native/Adapter.m')]
r=subprocess.run(objc,cwd=root,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
print(r.stdout);print('OBJC_TYPECHECK_EXIT',r.returncode);sys.exit(r.returncode)
