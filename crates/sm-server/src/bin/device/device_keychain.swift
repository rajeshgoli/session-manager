// Embedded in `sm device enroll`. Only the public key and CSR leave Keychain.
import Foundation
import Security

enum Failure: Error { case message(String) }
func check(_ status: OSStatus, _ operation: String) throws {
    guard status == errSecSuccess else {
        throw Failure.message("\(operation): \(SecCopyErrorMessageString(status, nil) as String? ?? String(status))")
    }
}
func der(_ tag: UInt8, _ bytes: Data) -> Data {
    var length = [UInt8]()
    var n = bytes.count
    repeat { length.insert(UInt8(n & 255), at: 0); n >>= 8 } while n > 0
    let encoded = bytes.count < 128 ? Data([UInt8(bytes.count)]) : Data([0x80 | UInt8(length.count)] + length)
    return Data([tag]) + encoded + bytes
}
func sequence(_ bytes: Data) -> Data { der(0x30, bytes) }
func pem(_ name: String, _ data: Data) -> String {
    let base64 = data.base64EncodedString(options: [.lineLength64Characters, .endLineWithLineFeed])
    return "-----BEGIN \(name)-----\n\(base64)\n-----END \(name)-----\n"
}
func trustedChrome() throws -> SecTrustedApplication {
    let paths = ["/Applications/Google Chrome.app", NSHomeDirectory() + "/Applications/Google Chrome.app"]
    guard let path = paths.first(where: { FileManager.default.fileExists(atPath: $0) }) else {
        throw Failure.message("Install Google Chrome before enrolling this computer")
    }
    var chrome: SecTrustedApplication?
    try check(SecTrustedApplicationCreateFromPath(path, &chrome), "Identify Google Chrome")
    guard let chrome = chrome else { throw Failure.message("Google Chrome identity unavailable") }
    return chrome
}
// Change signing entries only. Preserve other applications, operations, and
// password requirements; never turn a restricted list into all-app access.
func repairSigningAccess(_ access: SecAccess, _ label: String) throws -> Bool {
    let chrome = try trustedChrome()
    var chromeData: CFData?
    try check(SecTrustedApplicationCopyData(chrome, &chromeData), "Read Chrome identity")
    var entries: CFArray?
    try check(SecAccessCopyACLList(access, &entries), "Read key permissions")
    guard let entries = entries as? [SecACL] else { throw Failure.message("Key permissions unavailable") }
    var foundSigning = false, changed = false
    for entry in entries {
        let operations = SecACLCopyAuthorizations(entry) as! [String]
        guard operations.contains(kSecACLAuthorizationSign as String) else { continue }
        foundSigning = true
        var applications: CFArray?, description: CFString?
        var prompt = SecKeychainPromptSelector(rawValue: 0)
        try check(SecACLCopyContents(entry, &applications, &description, &prompt), "Read signing permissions")
        guard var trusted = applications as? [SecTrustedApplication] else {
            throw Failure.message("Refusing to modify a key that allows all applications")
        }
        let hasChrome = try trusted.contains { application in
            var data: CFData?
            try check(SecTrustedApplicationCopyData(application, &data), "Read trusted application")
            return data == chromeData
        }
        if !hasChrome { trusted.append(chrome) }
        if !hasChrome || description as String? != label {
            try check(SecACLSetContents(entry, trusted as CFArray, label as CFString, prompt), "Repair signing permissions")
            changed = true
        }
    }
    guard foundSigning else { throw Failure.message("Device key has no signing permissions") }
    return changed
}
func run() throws {
    guard (3...4).contains(CommandLine.arguments.count) else { throw Failure.message("Expected prepare|import|repair and device name") }
    let operation = CommandLine.arguments[1], name = CommandLine.arguments[2]
    guard name.range(of: "^[a-z0-9-]{1,32}$", options: .regularExpression) != nil else { throw Failure.message("Invalid device name") }
    let label = "li.rajeshgo.sm.device.\(name)"
    var login: SecKeychain?
    let path = CommandLine.arguments.count == 4 ? CommandLine.arguments[3] : NSHomeDirectory() + "/Library/Keychains/login.keychain-db"
    try check(SecKeychainOpen(path, &login), "Open login keychain")
    guard let keychain = login else { throw Failure.message("Login keychain unavailable") }
    let query: [String: Any] = [
        kSecClass as String: kSecClassKey,
        kSecAttrKeyClass as String: kSecAttrKeyClassPrivate,
        kSecAttrApplicationTag as String: Data(label.utf8),
        kSecMatchSearchList as String: [keychain],
        kSecReturnRef as String: true,
    ]
    var result: CFTypeRef?
    let found = SecItemCopyMatching(query as CFDictionary, &result)
    var error: Unmanaged<CFError>?
    let key: SecKey
    if found == errSecSuccess {
        key = result as! SecKey
    } else if found == errSecItemNotFound && operation == "prepare" {
        // The item label does not name security dialogs. Set an explicit
        // access descriptor and trust only this helper and the installed Chrome.
        let chrome = try trustedChrome()
        var creator: SecTrustedApplication?
        try check(SecTrustedApplicationCreateFromPath(nil, &creator), "Identify enrollment helper")
        guard let creator = creator else { throw Failure.message("Enrollment helper identity unavailable") }
        var access: SecAccess?
        try check(SecAccessCreate(label as CFString, [creator, chrome] as CFArray, &access), "Create device key permissions")
        guard let access = access else { throw Failure.message("Device key permissions unavailable") }
        let attributes: [String: Any] = [
            kSecAttrAccess as String: access,
            kSecAttrKeyType as String: kSecAttrKeyTypeECSECPrimeRandom,
            kSecAttrKeySizeInBits as String: 256,
            kSecUseKeychain as String: keychain,
            // The login keychain reads extractability only at the top level.
            kSecAttrIsExtractable as String: false,
            kSecPrivateKeyAttrs as String: [
                kSecAttrIsPermanent as String: true,
                kSecAttrIsExtractable as String: false,
                kSecAttrApplicationTag as String: Data(label.utf8),
                kSecAttrLabel as String: label,
            ],
        ]
        guard let generated = SecKeyCreateRandomKey(attributes as CFDictionary, &error) else {
            throw Failure.message("Create device key: \(String(describing: error?.takeRetainedValue()))")
        }
        key = generated
    } else {
        try check(found, "Find device key")
        throw Failure.message("Device key not found")
    }
    // Fail closed if the platform did not honor the non-exportable attribute.
    guard SecKeyCopyExternalRepresentation(key, &error) == nil else {
        throw Failure.message("Keychain did not enforce a non-exportable private key")
    }
    if operation == "repair" {
        let item = unsafeBitCast(key, to: SecKeychainItem.self)
        var access: SecAccess?
        try check(SecKeychainItemCopyAccess(item, &access), "Read device key permissions")
        guard let access = access else { throw Failure.message("Device key permissions unavailable") }
        if try repairSigningAccess(access, label) {
            // macOS owns any authorization dialog; no password enters this helper.
            try check(SecKeychainItemSetAccess(item, access), "Save device key permissions")
        }
        print("Repaired signing access for \(label). The existing key and certificate are unchanged.")
        return
    }
    guard let publicKey = SecKeyCopyPublicKey(key),
          let publicBytes = SecKeyCopyExternalRepresentation(publicKey, &error) as Data? else {
        throw Failure.message("Read public key failed")
    }
    if operation == "prepare" {
        let algorithm = sequence(der(6, Data([0x2a,0x86,0x48,0xce,0x3d,2,1])) + der(6, Data([0x2a,0x86,0x48,0xce,0x3d,3,1,7])))
        let spki = sequence(algorithm + der(3, Data([0]) + publicBytes))
        let subject = sequence(der(0x31, sequence(der(6, Data([0x55,4,3])) + der(12, Data(name.utf8)))))
        let request = sequence(der(2, Data([0])) + subject + spki + der(0xa0, Data()))
        guard let signature = SecKeyCreateSignature(key, .ecdsaSignatureMessageX962SHA256, request as CFData, &error) as Data? else {
            throw Failure.message("Sign certificate request failed")
        }
        let signatureAlgorithm = sequence(der(6, Data([0x2a,0x86,0x48,0xce,0x3d,4,3,2])))
        print(pem("CERTIFICATE REQUEST", sequence(request + signatureAlgorithm + der(3, Data([0]) + signature))), terminator: "")
    } else if operation == "import" {
        let input = String(decoding: FileHandle.standardInput.readDataToEndOfFile(), as: UTF8.self)
        let sections = input.components(separatedBy: "-----BEGIN CERTIFICATE-----").dropFirst()
        var leaf: SecCertificate?
        for section in sections {
            let encoded = section.components(separatedBy: "-----END CERTIFICATE-----")[0]
            guard let bytes = Data(base64Encoded: encoded, options: .ignoreUnknownCharacters),
                  let certificate = SecCertificateCreateWithData(nil, bytes as CFData) else {
                throw Failure.message("Invalid certificate")
            }
            if leaf == nil {
                guard let certKey = SecCertificateCopyKey(certificate),
                      SecKeyCopyExternalRepresentation(certKey, &error) as Data? == publicBytes else {
                    throw Failure.message("Server certificate does not match the local key")
                }
                leaf = certificate
            }
            let status = SecItemAdd([kSecClass as String: kSecClassCertificate,
                                    kSecValueRef as String: certificate,
                                    kSecUseKeychain as String: keychain] as CFDictionary, nil)
            if status != errSecDuplicateItem { try check(status, "Import certificate") }
        }
        guard let certificate = leaf else { throw Failure.message("No certificate received") }
        var identity: SecIdentity?
        try check(SecIdentityCreateWithCertificate(keychain, certificate, &identity), "Find certificate and key identity")
        guard identity != nil else { throw Failure.message("Certificate identity unavailable") }
    } else { throw Failure.message("Unknown operation") }
}
do { try run() } catch {
    FileHandle.standardError.write(Data("\(error)\n".utf8))
    exit(1)
}
