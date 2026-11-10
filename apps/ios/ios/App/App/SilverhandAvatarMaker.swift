import SilverhandNative
import SwiftUI
import UIKit

/// The native avatar editor. The page passes the starting config and uploads what
/// comes back; this only edits and rasterizes.
@MainActor
enum SilverhandAvatarMaker {
  struct Result {
    let config: AvatarConfig
    let png: Data
  }

  private static var library: AvatarLibrary?

  /// Calls `completion` exactly once: with the chosen avatar, or nil when dismissed.
  static func present(
    from host: UIViewController, parts: [String: Int], bg: Int,
    completion: @escaping (Result?) -> Void
  ) throws {
    let library = try self.library ?? AvatarLibrary.bundled()
    self.library = library

    var finished = false
    let finish: (Result?) -> Void = { result in
      guard !finished else { return }
      finished = true
      completion(result)
    }

    let controller = UIHostingController(rootView: AnyView(EmptyView()))
    let dismissal = Dismissal { finish(nil) }
    controller.rootView = AnyView(
      AvatarMakerView(
        library: library, initial: library.normalized(parts: parts, bg: bg),
        onCancel: { controller.dismiss(animated: true) { finish(nil) } },
        onSave: { config in
          let png = render(config, library: library)
          controller.dismiss(animated: true) { finish(Result(config: config, png: png)) }
        }))
    // A presented sheet takes its traits from the window, not the presenter; the theme
    // sets the shell's style on the presenter's ancestors, so carry it across.
    controller.overrideUserInterfaceStyle = host.traitCollection.userInterfaceStyle
    controller.modalPresentationStyle = .pageSheet
    controller.presentationController?.delegate = dismissal
    objc_setAssociatedObject(controller, &dismissalKey, dismissal, .OBJC_ASSOCIATION_RETAIN)
    if let sheet = controller.sheetPresentationController {
      sheet.detents = [.large()]
      sheet.prefersGrabberVisible = true
    }
    host.present(controller, animated: true)
  }

  /// 512 px keeps the 2–3 unit strokes crisp at the largest profile size, as on the web.
  static func render(_ config: AvatarConfig, library: AvatarLibrary, side: CGFloat = 512) -> Data {
    let format = UIGraphicsImageRendererFormat()
    format.scale = 1
    format.opaque = true
    return UIGraphicsImageRenderer(size: CGSize(width: side, height: side), format: format)
      .pngData { context in library.draw(config, in: context.cgContext, side: side) }
  }

  private static var dismissalKey = 0

  /// A swipe down dismisses without a callback from the editor itself.
  private final class Dismissal: NSObject, UIAdaptivePresentationControllerDelegate {
    let onDismiss: () -> Void
    init(onDismiss: @escaping () -> Void) { self.onDismiss = onDismiss }
    func presentationControllerDidDismiss(_ presentationController: UIPresentationController) {
      onDismiss()
    }
  }
}

private let backgroundTab = "background"


private struct AvatarMakerView: View {
  let library: AvatarLibrary
  let onCancel: () -> Void
  let onSave: (AvatarConfig) -> Void

  @State private var config: AvatarConfig
  @State private var tab: String

  init(
    library: AvatarLibrary, initial: AvatarConfig, onCancel: @escaping () -> Void,
    onSave: @escaping (AvatarConfig) -> Void
  ) {
    self.library = library
    self.onCancel = onCancel
    self.onSave = onSave
    _config = State(initialValue: initial)
    _tab = State(initialValue: library.categories.first?.id ?? backgroundTab)
  }

  private var tabs: [String] { library.categories.map(\.id) + [backgroundTab] }

  var body: some View {
    NavigationView {
      VStack(spacing: 16) {
        AvatarCanvas { context, side in library.draw(config, in: context, side: side) }
          .frame(width: 160, height: 160)
          .clipShape(RoundedRectangle(cornerRadius: 20, style: .continuous))
          .accessibilityLabel(NativeCopy.avatarPreview)

        HStack(spacing: 12) {
          Button {
            config = library.randomConfig()
          } label: {
            Label(NativeCopy.random, systemImage: "shuffle")
          }
          Button {
            config = library.defaultConfig()
          } label: {
            Label(NativeCopy.reset, systemImage: "arrow.counterclockwise")
          }
        }
        .buttonStyle(.bordered)
        .controlSize(.small)
        .tint(.primary)

        tabStrip
        grid
      }
      .padding(.top, 8)
      .navigationTitle(NativeCopy.makeAvatar)
      .navigationBarTitleDisplayMode(.inline)
      .toolbar {
        ToolbarItem(placement: .principal) {
          Text(NativeCopy.makeAvatar).font(.headline)
        }
        ToolbarItem(placement: .cancellationAction) {
          Button(NativeCopy.cancel, action: onCancel)
        }
        ToolbarItem(placement: .confirmationAction) {
          Button(NativeCopy.use) { onSave(config) }.font(.body.weight(.semibold))
        }
      }
    }
    .navigationViewStyle(.stack)
  }

  private var tabStrip: some View {
    ScrollView(.horizontal, showsIndicators: false) {
      HStack(spacing: 6) {
        ForEach(tabs, id: \.self) { id in
          let selected = id == tab
          Button {
            tab = id
          } label: {
            Text(NativeCopy.avatarCategory(id))
              .font(.subheadline.weight(selected ? .semibold : .regular))
              .foregroundColor(selected ? .primary : .secondary)
              .padding(.horizontal, 12)
              .padding(.vertical, 6)
              .background(Capsule().fill(selected ? Color(UIColor.tertiarySystemFill) : Color.clear))
          }
          .buttonStyle(.plain)
          .accessibilityAddTraits(selected ? .isSelected : [])
        }
      }
      .padding(.horizontal, 16)
    }
    // A scroll view draws past its frame into the safe area; on Duo that runs the
    // strip under the camera column, out of line with the content above and below.
    .clipped()
  }

  private var grid: some View {
    ScrollView {
      LazyVGrid(columns: [GridItem(.adaptive(minimum: 76), spacing: 12)], spacing: 12) {
        if tab == backgroundTab {
          ForEach(library.backgrounds.indices, id: \.self) { index in
            tile(label: "\(NativeCopy.avatarCategory(backgroundTab)) \(index + 1)", selected: config.bg == index) {
              config.bg = index
            } content: {
              Circle()
                .fill(Color(library.backgrounds[index]))
                .overlay(Circle().strokeBorder(Color(UIColor.separator)))
                .padding(18)
            }
          }
        } else if let category = library.categories.first(where: { $0.id == tab }) {
          let label = NativeCopy.avatarCategory(category.id)
          if category.optional {
            tile(label: NativeCopy.none, selected: config.parts[category.id] == -1) {
              config.parts[category.id] = -1
            } content: {
              Image(systemName: "nosign").font(.title3).foregroundColor(Color(white: 0.55))
            }
          }
          ForEach(category.parts.indices, id: \.self) { index in
            tile(label: "\(label) \(index + 1)", selected: config.parts[category.id] == index) {
              config.parts[category.id] = index
            } content: {
              AvatarCanvas { context, side in
                library.drawPart(category: category.id, index: index, in: context, side: side)
              }
              .padding(6)
            }
          }
        }
      }
      .padding(.horizontal, 16)
      .padding(.vertical, 4)
      .padding(.bottom, 12)
    }
  }

  private func tile<Content: View>(
    label: String, selected: Bool, action: @escaping () -> Void,
    @ViewBuilder content: () -> Content
  ) -> some View {
    Button(action: action) {
      Color.clear
        .aspectRatio(1, contentMode: .fit)
        .overlay(content())
        // Library art is black ink drawn for a light ground, so tiles stay light in a
        // dark sheet; the selection ring sits outside the tile to read on either.
        .background(
          RoundedRectangle(cornerRadius: 14, style: .continuous)
            .fill(Color(white: selected ? 1 : 0.95)))
        .overlay(
          RoundedRectangle(cornerRadius: 17, style: .continuous)
            .strokeBorder(selected ? Color.primary : .clear, lineWidth: 2)
            .padding(-3))
    }
    .buttonStyle(.plain)
    .accessibilityLabel(label)
    .accessibilityAddTraits(selected ? .isSelected : [])
  }
}

/// Draws library art into a square; the context is y-down like the SVG it came from.
struct AvatarCanvas: View {
  let draw: (CGContext, CGFloat) -> Void

  var body: some View {
    Canvas { context, size in
      let side = min(size.width, size.height)
      context.withCGContext { draw($0, side) }
    }
  }
}
