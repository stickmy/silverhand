import CoreFoundation
import Foundation

@MainActor
public final class ClientSession {
  public typealias Fetch = (URLRequest) async throws -> HTTPResult
  private let defaultServerURL: String?
  private let store: SessionStore
  public let contract: WireContract
  private let fetch: Fetch
  private let transport: NativeTransport
  private var record: SessionRecord?
  private var generation: UInt64 = 0
  private var storageBlocked = false
  private var active = true
  private struct Socket {
    let task: URLSessionWebSocketTask
    let generation: UInt64
  }
  private var sockets: [String: Socket] = [:]
  public var onSocket: (([String: Any]) -> Void)?
  public var onInvalidated: (() -> Void)?

  public init(
    store: SessionStore = KeychainSessionStore(), contract: WireContract? = nil,
    transport: NativeTransport = NativeTransport(), fetch: Fetch? = nil,
    defaultServerURL: String? = nil
  ) throws {
    self.defaultServerURL = try defaultServerURL.map(Self.origin)
    self.store = store
    self.contract = try contract ?? WireContract.bundled()
    self.transport = transport
    self.fetch = fetch ?? { try await transport.fetch($0) }
    self.record = try store.read()
    transport.socketOpened = { [weak self] task in
      Task { @MainActor in self?.opened(task) }
    }
    transport.socketClosed = { [weak self] task, code in
      Task { @MainActor in self?.closed(task, code: code) }
    }
  }

  deinit {
    transport.session.invalidateAndCancel()
  }

  public static func origin(_ raw: String) throws -> String {
    guard let parts = URLComponents(string: raw.trimmingCharacters(in: .whitespacesAndNewlines)),
      parts.scheme?.lowercased() == "https", let host = parts.host, !host.isEmpty,
      parts.user == nil, parts.password == nil, parts.query == nil, parts.fragment == nil,
      parts.path.isEmpty || parts.path == "/", !raw.contains("\\"),
      parts.port == nil || (1...65535).contains(parts.port!)
    else { throw ClientError.invalidURL }
    var normalized = parts
    normalized.scheme = "https"
    normalized.host = host.lowercased()
    normalized.path = ""
    if normalized.port == 443 { normalized.port = nil }
    guard let result = normalized.url?.absoluteString else { throw ClientError.invalidURL }
    return result
  }

  public func profile() throws -> [String: Any] {
    try checkStorage()
    if record == nil, let defaultServerURL {
      // A bundled default carries no credentials and never overwrites a saved origin.
      record = SessionRecord(serverURL: defaultServerURL, updatedAt: 0)
    }
    guard let record else { return ["server_url": NSNull()] }
    return ["server_url": record.serverURL, "updated_at": record.updatedAt]
  }

  private func checkStorage() throws { if storageBlocked { throw ClientError.storage(-1) } }
  private func check(_ captured: UInt64) throws {
    try checkStorage()
    if captured != generation { throw ClientError.staleSession }
  }
  private func request(
    origin: String, path: String, method: String = "GET", token: String? = nil, body: String? = nil,
    headers: [String: String] = [:]
  ) throws -> URLRequest {
    guard path.hasPrefix("/"), !path.hasPrefix("//"), !path.contains("\\"),
      let base = URL(string: origin), let url = URL(string: path, relativeTo: base)?.absoluteURL,
      url.scheme == base.scheme, url.host == base.host, url.port == base.port,
      url.user == nil, url.password == nil, url.fragment == nil
    else { throw ClientError.invalidPath }
    var result = URLRequest(url: url)
    result.httpMethod = method
    result.httpShouldHandleCookies = false
    result.setValue(String(contract.desktopApi), forHTTPHeaderField: contract.desktopApiEpochHeader)
    if let token { result.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization") }
    for (key, value) in headers
    where ["accept", "content-type", "if-match", "if-none-match", "idempotency-key"].contains(
      key.lowercased())
    {
      result.setValue(value, forHTTPHeaderField: key)
    }
    if let body {
      result.httpBody = Data(body.utf8)
      if result.value(forHTTPHeaderField: "Content-Type") == nil {
        result.setValue("application/json", forHTTPHeaderField: "Content-Type")
      }
    }
    return result
  }

  /// A single Keychain item atomically binds the token to its server. Failure
  /// leaves the old record intact and prevents retargeting to the new origin.
  private func commit(_ next: SessionRecord?) throws {
    try store.write(next)
    generation &+= 1
    closeAll()
    record = next
    storageBlocked = false
  }

  // Validate the entire discovery contract before the only durable commit.
  // Optional fields may be absent, but null and malformed advertisements are
  // rejected just as they are by the shared ServerInfoSchema.
  private func validServerInfo(_ info: [String: Any]) -> Bool {
    func integer(_ value: Any?) -> Bool {
      guard let number = value as? NSNumber,
        CFGetTypeID(number) != CFBooleanGetTypeID()
      else { return false }
      let value = number.doubleValue
      return value.isFinite && value >= 0 && value.rounded() == value
    }
    guard let signup = info["signup_enabled"] as? NSNumber,
      CFGetTypeID(signup) == CFBooleanGetTypeID()
    else { return false }
    if let version = info["outbound_protocol_version"], !integer(version) { return false }
    if let scope = info["bind_address_scope"] {
      guard let scope = scope as? String, ["loopback", "lan", "public"].contains(scope)
      else { return false }
    }
    if let features = info["features"] {
      guard let features = features as? [String: Any] else { return false }
      for key in ["signup_enabled", "password_recovery_enabled", "account_deletion_enabled"] {
        if let flag = features[key] {
          guard let flag = flag as? NSNumber, CFGetTypeID(flag) == CFBooleanGetTypeID()
          else { return false }
        }
      }
    }
    if let advertisement = info["desktop_api"] {
      guard let advertisement = advertisement as? [String: Any],
        integer(advertisement["current_epoch"]), integer(advertisement["minimum_client_epoch"])
      else { return false }
    }
    return true
  }

  public func saveProfile(_ raw: String) async throws -> [String: Any] {
    try checkStorage()
    let origin = try Self.origin(raw)
    let captured = generation
    let response = try await fetch(request(origin: origin, path: contract.serverInfo))
    try check(captured)
    guard response.status == 200, let info = response.json, validServerInfo(info) else {
      throw ClientError.invalidResponse
    }
    let now = Date().timeIntervalSince1970 * 1000
    let next = SessionRecord(
      serverURL: origin, updatedAt: now, token: origin == record?.serverURL ? record?.token : nil)
    try commit(next)
    return ["server_url": origin, "updated_at": now, "info": info]
  }

  public func clearProfile() throws -> Bool {
    let existed = record != nil
    try commit(nil)
    return existed
  }

  private func clearToken(notify: Bool) throws {
    guard var next = record else { return }
    next.token = nil
    // Even when Keychain denies cleanup, stop all networking until storage
    // is available. Never hydrate the stale disk token in this process.
    do { try commit(next) } catch {
      generation &+= 1
      closeAll()
      record?.token = nil
      storageBlocked = true
      throw error
    }
    if notify { onInvalidated?() }
  }

  public func authState() async throws -> [String: Any] {
    try checkStorage()
    guard let record else { throw ClientError.noServer }
    let captured = generation
    let info = try await fetch(request(origin: record.serverURL, path: contract.serverInfo))
    try check(captured)
    var result: [String: Any] = ["serverUrl": record.serverURL, "info": info.bridge()]
    if let token = record.token {
      let me = try await fetch(request(origin: record.serverURL, path: contract.me, token: token))
      try check(captured)
      if me.status == 401 || me.status == 403 { try clearToken(notify: true) }
      result["me"] = me.bridge()
    }
    return result
  }

  public func serverInfo() async throws -> [String: Any] {
    try checkStorage()
    guard let record else { throw ClientError.noServer }
    let captured = generation
    let info = try await fetch(request(origin: record.serverURL, path: contract.serverInfo))
    try check(captured)
    return info.bridge()
  }

  public func authenticate(action: String, body: String) async throws -> [String: Any] {
    try checkStorage()
    guard let record else { throw ClientError.noServer }
    guard action == "login" || action == "signup" else { throw ClientError.invalidPath }
    generation &+= 1
    closeAll()
    let captured = generation
    if record.updatedAt == 0 {
      let discovery = try await fetch(request(origin: record.serverURL, path: contract.serverInfo))
      try check(captured)
      guard discovery.status == 200, let info = discovery.json, validServerInfo(info) else {
        throw ClientError.invalidResponse
      }
    }
    let response = try await fetch(
      request(
        origin: record.serverURL, path: action == "login" ? contract.login : contract.signup,
        method: "POST", body: body))
    try check(captured)
    guard (200..<300).contains(response.status) else {
      return [
        "ok": false, "status": response.status,
        "statusText": HTTPURLResponse.localizedString(forStatusCode: response.status),
        "bodyText": String(data: response.data, encoding: .utf8) ?? "",
      ]
    }
    guard let body = response.json, let token = body["user_token"] as? String, !token.isEmpty,
      let user = body["user"] as? [String: Any], user["id"] is String
    else { throw ClientError.invalidResponse }
    try commit(
      SessionRecord(serverURL: record.serverURL, updatedAt: record.updatedAt, token: token))
    let fields = ["id", "name", "display_name", "avatar_url", "created_at", "updated_at"]
    let safeUser = user.filter { fields.contains($0.key) }
    return ["ok": true, "user": safeUser]
  }

  /// Anonymous recovery call on the selected server. It never touches the stored
  /// session, and a response that arrives after the server changed is refused.
  public func passwordRecovery(action: String, body: String) async throws -> [String: Any] {
    try checkStorage()
    guard let record else { throw ClientError.noServer }
    let path: String
    switch action {
    case "request": path = contract.passwordRecoveryRequest
    case "reset": path = contract.passwordRecoveryReset
    default: throw ClientError.invalidPath
    }
    let captured = generation
    let response = try await fetch(
      request(origin: record.serverURL, path: path, method: "POST", body: body))
    try check(captured)
    return ["status": response.status, "bodyText": String(data: response.data, encoding: .utf8) ?? ""]
  }

  public func logout() async throws {
    try checkStorage()
    let old = record
    try clearToken(notify: false)
    if let old, let token = old.token {
      _ = try? await fetch(
        request(origin: old.serverURL, path: contract.logout, method: "POST", token: token))
    }
  }

  public func proxy(
    path: String, method: String = "GET", body: String? = nil, headers: [String: String] = [:],
    binary: Bool = false
  ) async throws -> [String: Any] {
    try checkStorage()
    guard let record, let token = record.token else { throw ClientError.unauthorized }
    try checkAPIPath(path)
    let outgoing = try request(
      origin: record.serverURL, path: path, method: method, token: token, body: body,
      headers: headers)
    return try await send(outgoing, path: path, method: method, binary: binary)
  }

  /// A GET under the same rules as `proxy` whose body native consumes itself
  /// (an image for a native sheet), so it never crosses the bridge.
  public func download(path: String) async throws -> Data {
    try checkStorage()
    guard let record, let token = record.token else { throw ClientError.unauthorized }
    try checkAPIPath(path)
    let outgoing = try request(origin: record.serverURL, path: path, token: token)
    let response = try await exchange(outgoing, path: path, method: "GET")
    guard (200..<300).contains(response.status) else { throw ClientError.invalidResponse }
    return response.data
  }

  public struct UploadFile {
    public let fieldName: String
    public let filename: String
    public let contentType: String
    public let data: Data

    public init(fieldName: String, filename: String, contentType: String, data: Data) {
      self.fieldName = fieldName
      self.filename = filename
      self.contentType = contentType
      self.data = data
    }
  }

  /// A multipart POST under the same rules as `proxy`: an API path on the
  /// selected server, the stored bearer, and no result after the session changed.
  public func upload(
    path: String, fields: [(name: String, value: String)], files: [UploadFile]
  ) async throws -> [String: Any] {
    try checkStorage()
    guard let record, let token = record.token else { throw ClientError.unauthorized }
    try checkAPIPath(path)
    let boundary = "silverhand-\(UUID().uuidString)"
    var outgoing = try request(
      origin: record.serverURL, path: path, method: "POST", token: token,
      headers: ["Content-Type": "multipart/form-data; boundary=\(boundary)"])
    outgoing.httpBody = Self.multipart(boundary: boundary, fields: fields, files: files)
    return try await send(outgoing, path: path, method: "POST", binary: false)
  }

  /// Names and types become header text, so a quote or line break in them
  /// would end the header early. They are escaped the way browsers encode
  /// form data, which the server's parser reads back.
  static func multipart(
    boundary: String, fields: [(name: String, value: String)], files: [UploadFile]
  ) -> Data {
    func header(_ value: String) -> String {
      value
        .replacingOccurrences(of: "\"", with: "%22")
        .replacingOccurrences(of: "\r", with: "%0D")
        .replacingOccurrences(of: "\n", with: "%0A")
    }
    var body = Data()
    for field in fields {
      body.append(
        Data(
          "--\(boundary)\r\nContent-Disposition: form-data; name=\"\(header(field.name))\"\r\n\r\n"
            .utf8))
      body.append(Data(field.value.utf8))
      body.append(Data("\r\n".utf8))
    }
    for file in files {
      body.append(
        Data(
          ("--\(boundary)\r\nContent-Disposition: form-data; name=\"\(header(file.fieldName))\"; "
            + "filename=\"\(header(file.filename))\"\r\n"
            + "Content-Type: \(header(file.contentType))\r\n\r\n").utf8))
      body.append(file.data)
      body.append(Data("\r\n".utf8))
    }
    body.append(Data("--\(boundary)--\r\n".utf8))
    return body
  }

  private func checkAPIPath(_ path: String) throws {
    guard let decoded = path.removingPercentEncoding, decoded.hasPrefix("/api/"),
      !decoded.contains("\\"),
      !decoded.split(separator: "/").contains(where: { $0 == "." || $0 == ".." })
    else { throw ClientError.invalidPath }
  }

  private func send(
    _ outgoing: URLRequest, path: String, method: String, binary: Bool
  ) async throws -> [String: Any] {
    let response = try await exchange(outgoing, path: path, method: method)
    return response.bridge(binary: binary && (200..<300).contains(response.status))
  }

  private func exchange(
    _ outgoing: URLRequest, path: String, method: String
  ) async throws -> HTTPResult {
    let captured = generation
    let response = try await fetch(outgoing)
    try check(captured)
    if response.status == 401
      || (path == contract.mePassword && method == "POST" && (200..<300).contains(response.status))
    {
      try clearToken(notify: true)
    }
    return response
  }

  public func setActive(_ value: Bool) {
    active = value
    if !value { closeAll() }
  }

  public func openSocket(id: String, url: String) throws {
    try checkStorage()
    guard active else { throw ClientError.suspended }
    guard let record, let token = record.token else { throw ClientError.unauthorized }
    var request = try request(origin: record.serverURL, path: contract.eventWebSocketPath)
    var components = URLComponents(url: request.url!, resolvingAgainstBaseURL: false)!
    components.scheme = "wss"
    guard components.url?.absoluteString == url else { throw ClientError.invalidURL }
    guard !token.contains(";"), !token.contains("\r"), !token.contains("\n") else {
      throw ClientError.invalidResponse
    }
    request.url = components.url
    request.setValue(record.serverURL, forHTTPHeaderField: "Origin")
    request.setValue(
      "\(contract.eventWebSocketSessionCookie)=\(token)", forHTTPHeaderField: "Cookie")
    closeSocket(id: id)
    let task = transport.session.webSocketTask(with: request)
    sockets[id] = Socket(task: task, generation: generation)
    task.resume()
  }

  private func opened(_ task: URLSessionWebSocketTask) {
    guard let (id, socket) = sockets.first(where: { $0.value.task === task }),
      socket.generation == generation
    else {
      task.cancel(with: .goingAway, reason: nil)
      return
    }
    onSocket?(["id": id, "type": "open"])
    Task { @MainActor [weak self] in
      while let self, self.sockets[id]?.task === task, self.generation == socket.generation {
        do {
          let message = try await task.receive()
          guard self.sockets[id]?.task === task, self.generation == socket.generation else {
            return
          }
          if case .string(let data) = message {
            self.onSocket?(["id": id, "type": "message", "data": data])
          }
        } catch {
          self.closed(task, code: 1006)
          return
        }
      }
    }
  }

  private func closed(_ task: URLSessionWebSocketTask, code: Int) {
    guard let (id, socket) = sockets.first(where: { $0.value.task === task }) else { return }
    sockets.removeValue(forKey: id)
    if socket.generation == generation, (task.response as? HTTPURLResponse)?.statusCode == 401 {
      try? clearToken(notify: true)
    }
    onSocket?(["id": id, "type": "close", "code": code])
  }
  public func sendSocket(id: String, data: String) async throws {
    guard let socket = sockets[id], socket.generation == generation else {
      throw ClientError.staleSession
    }
    try await socket.task.send(.string(data))
  }
  public func closeSocket(id: String) {
    guard let socket = sockets.removeValue(forKey: id) else { return }
    socket.task.cancel(with: .normalClosure, reason: nil)
    onSocket?(["id": id, "type": "close", "code": 1000])
  }
  private func closeAll() { for id in Array(sockets.keys) { closeSocket(id: id) } }
}
