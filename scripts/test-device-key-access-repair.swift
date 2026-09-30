// Appended to the helper's functions by the disposable enrollment harness.
func expect(_ condition: Bool, _ message: String) throws {
    if !condition { throw Failure.message(message) }
}
func snapshot(_ access: SecAccess) throws -> [(SecACL, [String], [Data]?, String?, SecKeychainPromptSelector)] {
    var entries: CFArray?
    try check(SecAccessCopyACLList(access, &entries), "Read fixture permissions")
    return try (entries as! [SecACL]).map { entry in
        var applications: CFArray?, description: CFString?
        var prompt = SecKeychainPromptSelector(rawValue: 0)
        try check(SecACLCopyContents(entry, &applications, &description, &prompt), "Read fixture entry")
        let apps = try (applications as? [SecTrustedApplication]).map { apps in
            try apps.map { app -> Data in
                var data: CFData?
                try check(SecTrustedApplicationCopyData(app, &data), "Read fixture app")
                return data! as Data
            }
        }
        return (entry, SecACLCopyAuthorizations(entry) as! [String], apps, description as String?, prompt)
    }
}
do {
    var creator: SecTrustedApplication?, other: SecTrustedApplication?, access: SecAccess?
    try check(SecTrustedApplicationCreateFromPath(nil, &creator), "Identify fixture interpreter")
    try check(SecTrustedApplicationCreateFromPath("/usr/bin/true", &other), "Identify unrelated trusted app")
    try expect(try isSwiftInterpreter(creator!), "Legacy fixture must run through Swift")
    var interpreterData: CFData?
    try check(SecTrustedApplicationCopyData(creator!, &interpreterData), "Read interpreter identity")
    try check(SecAccessCreate("<key>" as CFString, [creator!, other!] as CFArray, &access), "Create legacy permissions")
    let legacy = access!, label = "li.rajeshgo.sm.device.qa-device"
    let before = try snapshot(legacy)
    try expect(try repairSigningAccess(legacy, label), "Legacy permissions must need repair")
    let after = try snapshot(legacy)
    try expect(before.count == after.count, "Do not replace permission entries")
    for (old, new) in zip(before, after) {
        try expect(old.1 == new.1 && old.4 == new.4, "Preserve operations and password requirements")
        if old.1.contains(kSecACLAuthorizationSign as String) {
            try expect(new.3 == label, "Use the device label in signing prompts")
            try expect(new.2?.count == old.2!.count, "Replace interpreter with Chrome")
            try expect(!new.2!.contains(interpreterData! as Data), "Remove legacy interpreter trust")
            let retained = old.2!.filter { $0 != interpreterData! as Data }
            try expect(retained.allSatisfy { new.2!.contains($0) }, "Preserve unrelated trusted apps")
            var chromeData: CFData?
            try check(SecTrustedApplicationCopyData(try trustedChrome(), &chromeData), "Read expected Chrome identity")
            try expect(new.2!.contains(chromeData! as Data), "Trust the installed Chrome")
        } else {
            try expect(old.2 == new.2 && old.3 == new.3, "Do not change unrelated permissions")
        }
    }
    try expect(try !repairSigningAccess(legacy, label), "Repair must be idempotent")
    let signing = after.first { $0.1.contains(kSecACLAuthorizationSign as String) }!
    try check(SecACLSetContents(signing.0, nil, "<key>" as CFString, signing.4), "Set all-app fixture")
    var rejected = false
    do { _ = try repairSigningAccess(legacy, label) } catch { rejected = true }
    try expect(rejected, "Refuse unexpected all-app signing permissions")
    print("Legacy key permission repair, idempotency, and restricted access checks passed.")
} catch {
    FileHandle.standardError.write(Data("\(error)\n".utf8)); exit(1)
}
