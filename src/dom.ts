import { icon, type IconName } from "./icons";

export type Child = Node | string | null | undefined | false;

export interface ElOptions {
  class?: string;
  text?: string;
  id?: string;
  title?: string;
  /** Attributes applied verbatim — used for `aria-*`, `role`, `type`, `hidden`, ... */
  attrs?: Record<string, string>;
  children?: Child[];
}

export function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  options: ElOptions = {},
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (options.class !== undefined) {
    node.className = options.class;
  }
  if (options.text !== undefined) {
    node.textContent = options.text;
  }
  if (options.id !== undefined) {
    node.id = options.id;
  }
  if (options.title !== undefined) {
    node.title = options.title;
  }
  if (options.attrs !== undefined) {
    for (const [name, value] of Object.entries(options.attrs)) {
      node.setAttribute(name, value);
    }
  }
  if (options.children !== undefined) {
    for (const child of options.children) {
      if (child !== null && child !== undefined && child !== false) {
        node.append(child);
      }
    }
  }
  return node;
}

export interface ButtonOptions {
  class?: string;
  icon?: IconName;
  title?: string;
  ariaLabel?: string;
  disabled?: boolean;
  onClick?: () => void;
}

/** Text button, optionally with a leading icon. */
export function button(label: string, options: ButtonOptions = {}): HTMLButtonElement {
  const node = el("button", { class: options.class ?? "btn", attrs: { type: "button" } });
  if (options.icon !== undefined) {
    node.append(icon(options.icon, 16));
  }
  if (label.length > 0) {
    node.append(el("span", { text: label }));
  }
  if (options.title !== undefined) {
    node.title = options.title;
  }
  if (options.ariaLabel !== undefined) {
    node.setAttribute("aria-label", options.ariaLabel);
  }
  node.disabled = options.disabled ?? false;
  if (options.onClick !== undefined) {
    node.addEventListener("click", options.onClick);
  }
  return node;
}

/** Icon-only button; the label lives in `aria-label`/`title` for screen readers. */
export function iconButton(
  iconName: IconName,
  ariaLabel: string,
  options: ButtonOptions = {},
): HTMLButtonElement {
  return button("", {
    ...options,
    class: options.class ?? "btn btn-icon",
    icon: iconName,
    ariaLabel,
    title: options.title ?? ariaLabel,
  });
}

export interface FieldOptions {
  hint?: string;
  class?: string;
}

/** `<label>` wrapping a single control, with an optional hint line. */
export function field(labelText: string, control: HTMLElement, options: FieldOptions = {}): HTMLLabelElement {
  const children: Child[] = [
    el("span", { class: "field-label", text: labelText }),
    control,
    options.hint !== undefined ? el("span", { class: "field-hint", text: options.hint }) : null,
  ];
  return el("label", { class: options.class ?? "field", children });
}

/** Checkbox row: control first, label text after it. */
export function checkField(
  labelText: string,
  control: HTMLInputElement,
  options: FieldOptions = {},
): HTMLLabelElement {
  const children: Child[] = [
    control,
    el("span", { class: "field-label", text: labelText }),
    options.hint !== undefined ? el("span", { class: "field-hint", text: options.hint }) : null,
  ];
  return el("label", { class: options.class ?? "check-row", children });
}


