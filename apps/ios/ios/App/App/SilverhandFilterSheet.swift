import SwiftUI
import UIKit

/// What the page declares for the filter sheet; every string is the page's.
struct FilterSheetState {
  struct Option: Identifiable {
    let id, label: String
    let detail, group: String?
    /// Present when the option wears an avatar; `picture` is nil for an initial.
    let avatar: Avatar?

    struct Avatar {
      let picture: UIImage?
    }
  }

  struct Field: Identifiable {
    let id, label, selected: String
    let options: [Option]
    let note, searchPlaceholder: String?

    var selectedOption: Option? { options.first { $0.id == selected } }
  }

  var title, doneLabel: String
  var fields: [Field]
  var resetLabel: String?

  init?(_ raw: [String: Any]) {
    guard let title = raw["title"] as? String, let doneLabel = raw["doneLabel"] as? String,
      let fields = raw["fields"] as? [[String: Any]]
    else { return nil }
    self.title = title
    self.doneLabel = doneLabel
    self.resetLabel = raw["resetLabel"] as? String
    self.fields = fields.compactMap { field in
      guard let id = field["id"] as? String, let label = field["label"] as? String,
        let selected = field["selected"] as? String,
        let options = field["options"] as? [[String: Any]]
      else { return nil }
      return Field(
        id: id, label: label, selected: selected,
        options: options.compactMap { option in
          guard let id = option["id"] as? String, let label = option["label"] as? String else {
            return nil
          }
          let avatar = (option["avatar"] as? [String: Any]).map { avatar in
            Option.Avatar(
              picture: (avatar["image"] as? String).flatMap { Data(base64Encoded: $0) }.flatMap {
                UIImage(data: $0, scale: 3)
              })
          }
          return Option(
            id: id, label: label, detail: option["detail"] as? String,
            group: option["group"] as? String, avatar: avatar)
        },
        note: field["note"] as? String, searchPlaceholder: field["searchPlaceholder"] as? String)
    }
  }
}

@MainActor
final class FilterSheetModel: ObservableObject {
  @Published var state: FilterSheetState
  let send: ([String: Any]) -> Void

  init(state: FilterSheetState, send: @escaping ([String: Any]) -> Void) {
    self.state = state
    self.send = send
  }
}

/// Presents the filter sheet and keeps it on the page's latest state. The page
/// applies each pick itself and answers with the new state, so a field that
/// depends on another (project, then channel) updates in place.
@MainActor
final class FilterSheetPresenter {
  var onEvent: (([String: Any]) -> Void)?

  private var model: FilterSheetModel?
  private weak var controller: UIViewController?
  private var dismissal: Dismissal?

  func show(_ state: FilterSheetState, from host: UIViewController) -> Bool {
    if let model, controller?.presentingViewController != nil {
      model.state = state
      return true
    }
    guard host.presentedViewController == nil else { return false }

    let model = FilterSheetModel(state: state) { [weak self] event in self?.onEvent?(event) }
    let controller = UIHostingController(rootView: AnyView(EmptyView()))
    controller.rootView = AnyView(
      FilterSheetView(model: model, onDone: { [weak self, weak controller] in
        // A dismissal in code does not reach the presentation delegate.
        controller?.dismiss(animated: true) { self?.dismissed() }
      }))
    let dismissal = Dismissal { [weak self] in self?.dismissed() }
    // A presented sheet takes its traits from the window, not the presenter; the theme
    // sets the shell's style on the presenter's ancestors, so carry it across.
    controller.overrideUserInterfaceStyle = host.traitCollection.userInterfaceStyle
    controller.modalPresentationStyle = .pageSheet
    controller.presentationController?.delegate = dismissal
    if let sheet = controller.sheetPresentationController {
      sheet.detents = [.medium(), .large()]
      sheet.prefersGrabberVisible = true
    }
    self.model = model
    self.controller = controller
    self.dismissal = dismissal
    host.present(controller, animated: true)
    return true
  }

  func hide() {
    guard let controller, controller.presentingViewController != nil else { return }
    model = nil
    controller.dismiss(animated: true)
  }

  private func dismissed() {
    model = nil
    onEvent?(["kind": "dismiss"])
  }

  /// A swipe down.
  private final class Dismissal: NSObject, UIAdaptivePresentationControllerDelegate {
    let onDismiss: () -> Void
    init(onDismiss: @escaping () -> Void) { self.onDismiss = onDismiss }
    func presentationControllerDidDismiss(_ presentationController: UIPresentationController) {
      onDismiss()
    }
  }
}

private struct FilterSheetView: View {
  @ObservedObject var model: FilterSheetModel
  let onDone: () -> Void

  var body: some View {
    NavigationView {
      Form {
        Section {
          ForEach(model.state.fields) { field in
            NavigationLink {
              FilterOptionList(model: model, fieldID: field.id)
            } label: {
              HStack {
                Text(field.label)
                Spacer(minLength: 12)
                if let option = field.selectedOption {
                  if let avatar = option.avatar {
                    FilterAvatar(avatar: avatar, label: option.label, side: 20)
                  }
                  Text(option.label).foregroundColor(.secondary).lineLimit(1)
                }
              }
            }
          }
        }
        if let reset = model.state.resetLabel {
          Section {
            Button(reset) { model.send(["kind": "reset"]) }
          }
        }
      }
      .navigationTitle(model.state.title)
      .navigationBarTitleDisplayMode(.inline)
      .toolbar {
        ToolbarItem(placement: .confirmationAction) {
          Button(model.state.doneLabel, action: onDone).font(.body.weight(.semibold))
        }
      }
    }
    .navigationViewStyle(.stack)
  }
}

/// A field's options. It reads the field from the model by id, so the page's
/// answer to a pick (a new selection, a narrowed list) shows here at once.
private struct FilterOptionList: View {
  @ObservedObject var model: FilterSheetModel
  let fieldID: String
  @State private var query = ""
  @Environment(\.dismiss) private var dismiss

  /// Short lists read at a glance; a search field would only push them down.
  private static let searchFrom = 12

  var body: some View {
    let field = model.state.fields.first { $0.id == fieldID }
    let list = List {
      if let field {
        ForEach(Array(sections(field.options).enumerated()), id: \.offset) { _, section in
          Section(header: section.title.isEmpty ? nil : Text(section.title)) {
            ForEach(section.options) { option in row(option, field: field) }
          }
        }
        if let note = field.note {
          Section { Text(note).foregroundColor(.secondary) }
        }
      }
    }
    .navigationTitle(field?.label ?? "")
    .navigationBarTitleDisplayMode(.inline)

    if let field, field.options.count >= Self.searchFrom {
      list.searchable(
        text: $query, placement: .navigationBarDrawer(displayMode: .always),
        prompt: field.searchPlaceholder.map { Text($0) })
    } else {
      list
    }
  }

  private func row(_ option: FilterSheetState.Option, field: FilterSheetState.Field) -> some View {
    Button {
      model.send(["kind": "select", "field": field.id, "option": option.id])
      dismiss()
    } label: {
      HStack(spacing: 10) {
        if let avatar = option.avatar {
          FilterAvatar(avatar: avatar, label: option.label, side: 28)
        }
        VStack(alignment: .leading, spacing: 1) {
          Text(option.label).foregroundColor(.primary).lineLimit(1)
          if let detail = option.detail {
            Text(detail).font(.footnote).foregroundColor(.secondary).lineLimit(1)
          }
        }
        Spacer(minLength: 8)
        if option.id == field.selected {
          Image(systemName: "checkmark").font(.body.weight(.semibold)).foregroundColor(.accentColor)
        }
      }
    }
    .accessibilityAddTraits(option.id == field.selected ? .isSelected : [])
  }

  private struct OptionSection {
    let title: String
    let options: [FilterSheetState.Option]
  }

  /// Runs of options under their group headings, in the page's order.
  private func sections(_ options: [FilterSheetState.Option]) -> [OptionSection] {
    let needle = query.trimmingCharacters(in: .whitespaces)
    let shown =
      needle.isEmpty
      ? options
      : options.filter {
        $0.label.localizedCaseInsensitiveContains(needle)
          || ($0.detail?.localizedCaseInsensitiveContains(needle) ?? false)
      }
    var result: [OptionSection] = []
    for option in shown {
      let title = option.group ?? ""
      if let last = result.last, last.title == title {
        result[result.count - 1] = OptionSection(title: title, options: last.options + [option])
      } else {
        result.append(OptionSection(title: title, options: [option]))
      }
    }
    return result
  }
}

private struct FilterAvatar: View {
  let avatar: FilterSheetState.Option.Avatar
  let label: String
  let side: CGFloat

  var body: some View {
    Group {
      if let picture = avatar.picture {
        Image(uiImage: picture).resizable().scaledToFill()
      } else {
        Text(label.prefix(1).uppercased())
          .font(.system(size: side * 0.45, weight: .semibold))
          .foregroundColor(.secondary)
          .frame(maxWidth: .infinity, maxHeight: .infinity)
          .background(Color(.tertiarySystemFill))
      }
    }
    .frame(width: side, height: side)
    .clipShape(Circle())
    .accessibilityHidden(true)
  }
}
