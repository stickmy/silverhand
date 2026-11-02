import {
  AlarmClock,
  ArrowDown,
  ArrowDownToLine,
  ArrowDownUp,
  ArrowLeft,
  ArrowRight,
  ArrowRightToLine,
  ArrowUp,
  ArrowUpRight,
  AtSign,
  Ban,
  Bell,
  BellOff,
  BellPlus,
  BellRing,
  Bot,
  Brain,
  CalendarClock,
  Camera,
  Check,
  CheckCheck,
  ChevronDown,
  ChevronLeft,
  ChevronRight,
  ChevronUp,
  Circle,
  CircleAlert,
  CircleCheck,
  CircleX,
  ClipboardCopy,
  Clock,
  Code,
  CodeXml,
  CornerDownLeft,
  Cpu,
  Dices,
  Download,
  Ellipsis,
  EllipsisVertical,
  Eraser,
  ExternalLink,
  Eye,
  EyeOff,
  FaceSlightlySmilingPlus,
  Globe,
  GripVertical,
  Hash,
  Hourglass,
  IdCard,
  Inbox,
  Languages,
  Info,
  Layers,
  Link2,
  Maximize2,
  Minimize2,
  Palette,
  PenLine,
  Pencil,
  Play,
  Plug,
  Plus,
  Power,
  Quote,
  RefreshCcw,
  RefreshCw,
  Repeat,
  RotateCcw,
  RotateCcwClock,
  RotateCw,
  Save,
  Search,
  Server,
  Settings2,
  Share,
  Share2,
  ShieldCheck,
  ShieldOff,
  SlidersHorizontal,
  Sprout,
  SquareMousePointer,
  Terminal,
  TextQuote,
  Timer,
  TriangleAlert,
  Unlink,
  Unplug,
  User,
  UserMinus,
  UserPlus,
  Users,
  Workflow,
  WifiOff,
  Wrench,
  X,
  ZoomIn,
  ZoomOut,
  Upload,
} from "lucide";
import { createIcon, type IconNode } from "./icon";

const icon = (name: string, node: unknown) => createIcon(name, node as IconNode);

export const AddMemberIcon = icon("add-member", UserPlus);
export const AddReactionIcon = icon("add-reaction", FaceSlightlySmilingPlus);
export const AlarmIcon = icon("alarm", AlarmClock);
export const AppearanceIcon = icon("appearance", Palette);
export const ArrowDownIcon = icon("arrow-down", ArrowDown);
export const ArrowDownToLineIcon = icon("arrow-down-to-line", ArrowDownToLine);
export const ArrowLeftIcon = icon("arrow-left", ArrowLeft);
export const ArrowRightIcon = icon("arrow-right", ArrowRight);
export const ArrowRightToLineIcon = icon("arrow-right-to-line", ArrowRightToLine);
export const ArrowUpIcon = icon("arrow-up", ArrowUp);
export const ArrowUpRightIcon = icon("arrow-up-right", ArrowUpRight);
export const BellIcon = icon("bell", Bell);
export const BellOffIcon = icon("bell-off", BellOff);
export const BellRingIcon = icon("bell-ring", BellRing);
export const BellPlusIcon = icon("bell-plus", BellPlus);
export const BotIcon = icon("bot", Bot);
export const CameraIcon = icon("camera", Camera);
export const UploadIcon = icon("upload", Upload);
export const CancelledIcon = icon("cancelled", Ban);
export const CheckIcon = icon("check", Check);
export const CheckAllIcon = icon("check-all", CheckCheck);
export const ChevronDownIcon = icon("chevron-down", ChevronDown);
export const ChevronLeftIcon = icon("chevron-left", ChevronLeft);
export const ChevronRightIcon = icon("chevron-right", ChevronRight);
export const ChevronUpIcon = icon("chevron-up", ChevronUp);
export const CircleIcon = icon("circle", Circle);
export const ClearIcon = icon("clear", Eraser);
export const ClockIcon = icon("clock", Clock);
export const CloseIcon = icon("close", X);
export const CodeFontIcon = icon("code-font", CodeXml);
export const CodeIcon = icon("code", Code);
export const CollapseIcon = icon("collapse", Minimize2);
export const CopyMarkdownIcon = icon("copy-markdown", ClipboardCopy);
export const CpuIcon = icon("cpu", Cpu);
export const CreatedByIcon = icon("created-by", PenLine);
export const DiagramIcon = icon("diagram", Workflow);
export const DisconnectedIcon = icon("disconnected", Unplug);
export const DownloadIcon = icon("download", Download);
export const DragHandleIcon = icon("drag-handle", GripVertical);
export const EditIcon = icon("edit", Pencil);
export const EnterIcon = icon("enter", CornerDownLeft);
/** An empty list or collection: an empty tray. */
export const EmptyIcon = icon("empty", Inbox);
export const ErrorIcon = icon("error", CircleAlert);
export const ExpandIcon = icon("expand", Maximize2);
export const FailedIcon = icon("failed", CircleX);
export const FiltersIcon = icon("filters", SlidersHorizontal);
const chatBubble: IconNode = [
  [
    "path",
    {
      d: "M21.5 12C21.5 17.2467 17.2467 21.5 12 21.5C10.3719 21.5 8.8394 21.0904 7.5 20.3687C5.63177 19.362 4.37462 20.2979 3.26592 20.4658C3.09774 20.4913 2.93024 20.4302 2.80997 20.31C2.62741 20.1274 2.59266 19.8451 2.6935 19.6074C3.12865 18.5818 3.5282 16.6382 2.98341 15C2.6698 14.057 2.5 13.0483 2.5 12C2.5 6.75329 6.75329 2.5 12 2.5C17.2467 2.5 21.5 6.75329 21.5 12Z",
    },
  ],
  [
    "path",
    {
      d: "M12.1257 12H12.0007M8.125 12H8M16.125 12H16M12.2507 12C12.2507 12.1381 12.1388 12.25 12.0007 12.25C11.8627 12.25 11.7507 12.1381 11.7507 12C11.7507 11.8619 11.8627 11.75 12.0007 11.75C12.1388 11.75 12.2507 11.8619 12.2507 12ZM8.25 12C8.25 12.1381 8.13807 12.25 8 12.25C7.86193 12.25 7.75 12.1381 7.75 12C7.75 11.8619 7.86193 11.75 8 11.75C8.13807 11.75 8.25 11.8619 8.25 12ZM16.25 12C16.25 12.1381 16.1381 12.25 16 12.25C15.8619 12.25 15.75 12.1381 15.75 12C15.75 11.8619 15.8619 11.75 16 11.75C16.1381 11.75 16.25 11.8619 16.25 12Z",
    },
  ],
];

export const ChatsIcon = icon("chats", chatBubble);
/** A message search result: the same chat bubble. */
export const MessageResultIcon = icon("message-result", chatBubble);
export const AttachmentIcon = icon("attachment", [
  ["path", { d: "M8 18H16" }],
  ["path", { d: "M8 14H16" }],
  ["path", { d: "M8 10H10" }],
  [
    "path",
    {
      d: "M13 2.5V3C13 5.82843 13 7.24264 13.8787 8.12132C14.7574 9 16.1716 9 19 9H19.5M20 10.6569V14C20 17.7712 20 19.6569 18.8284 20.8284C17.6569 22 15.7712 22 12 22C8.22876 22 6.34315 22 5.17157 20.8284C4 19.6569 4 17.7712 4 14V9.45584C4 6.21082 4 4.58831 4.88607 3.48933C5.06508 3.26731 5.26731 3.06508 5.48933 2.88607C6.58831 2 8.21082 2 11.4558 2C12.1614 2 12.5141 2 12.8372 2.11401C12.9044 2.13772 12.9702 2.165 13.0345 2.19575C13.3436 2.34355 13.593 2.593 14.0919 3.09188L18.8284 7.82843C19.4065 8.40649 19.6955 8.69552 19.8478 9.06306C20 9.4306 20 9.83935 20 10.6569Z",
    },
  ],
]);
/** The composer's attach-a-file control. */
export const AttachFileIcon = icon("attach-file", [
  ["path", { d: "M17.495 15V22M21 18.495L14 18.495" }],
  ["path", { d: "M13 2.5V3C13 5.82843 13 7.24264 13.8787 8.12132C14.7574 9 16.1716 9 19 9H19.5" }],
  [
    "path",
    {
      d: "M20 12V10.6569C20 9.83935 20 9.4306 19.8478 9.06306C19.6955 8.69552 19.4065 8.40649 18.8284 7.82843L14.0919 3.09188C13.593 2.593 13.3436 2.34355 13.0345 2.19575C12.9702 2.165 12.9044 2.13772 12.8372 2.11401C12.5141 2 12.1614 2 11.4558 2C8.21082 2 6.58831 2 5.48933 2.88607C5.26731 3.06508 5.06508 3.26731 4.88607 3.48933C4 4.58831 4 6.21082 4 9.45584V14C4 17.7712 4 19.6569 5.17157 20.8284C6.11466 21.7715 7.52043 21.9554 10 21.9913H12",
    },
  ],
]);
export const ActivityIcon = icon("activity", [
  ["path", { d: "M9 22H15C20 22 22 20 22 15V9C22 4 20 2 15 2H9C4 2 2 4 2 9V15C2 20 4 22 9 22Z" }],
  [
    "path",
    {
      d: "M7.33008 14.49L9.71008 11.4C10.0501 10.96 10.6801 10.88 11.1201 11.22L12.9501 12.66C13.3901 13 14.0201 12.92 14.3601 12.49L16.6701 9.51001",
    },
  ],
]);
export const TaskIcon = icon("task", [
  ["path", { d: "M12.37 8.87988H17.62" }],
  ["path", { d: "M6.38 8.87988L7.13 9.62988L9.38 7.37988" }],
  ["path", { d: "M12.37 15.8799H17.62" }],
  ["path", { d: "M6.38 15.8799L7.13 16.6299L9.38 14.3799" }],
  ["path", { d: "M9 22H15C20 22 22 20 22 15V9C22 4 20 2 15 2H9C4 2 2 4 2 9V15C2 20 4 22 9 22Z" }],
]);
export const FontIcon = icon("font", [
  [
    "path",
    {
      d: "M4 7.95V7C4 5.1 4 4.15 4.59 3.59C5.15 3 6.1 3 8 3H16C17.9 3 18.85 3 19.41 3.59C20 4.15 20 5.1 20 7V7.95",
    },
  ],
  ["path", { d: "M12 3V21M7 21H17" }],
]);
export const DirectMessageIcon = icon("direct-message", [
  [
    "path",
    {
      d: "M18.47 16.83L18.86 19.99C18.96 20.82 18.07 21.4 17.36 20.97L13.17 18.48C12.71 18.48 12.26 18.45 11.82 18.39C12.56 17.52 13 16.42 13 15.23C13 12.39 10.54 10.09 7.49997 10.09C6.33997 10.09 5.26997 10.42 4.37997 11C4.34997 10.75 4.33997 10.5 4.33997 10.24C4.33997 5.68999 8.28997 2 13.17 2C18.05 2 22 5.68999 22 10.24C22 12.94 20.61 15.33 18.47 16.83Z",
    },
  ],
  [
    "path",
    {
      d: "M13 15.23C13 16.42 12.56 17.5201 11.82 18.3901C10.83 19.5901 9.26 20.36 7.5 20.36L4.89 21.91C4.45 22.18 3.89 21.81 3.95 21.3L4.2 19.3301C2.86 18.4001 2 16.91 2 15.23C2 13.47 2.94 11.9201 4.38 11.0001C5.27 10.4201 6.34 10.0901 7.5 10.0901C10.54 10.0901 13 12.39 13 15.23Z",
    },
  ],
]);

export const SettingsIcon = icon("settings", [
  ["circle", { cx: "12", cy: "12", r: "3" }],
  [
    "path",
    {
      d: "M13.7654 2.15224C13.3978 2 12.9319 2 12 2C11.0681 2 10.6022 2 10.2346 2.15224C9.74457 2.35523 9.35522 2.74458 9.15223 3.23463C9.05957 3.45834 9.0233 3.7185 9.00911 4.09799C8.98826 4.65568 8.70226 5.17189 8.21894 5.45093C7.73564 5.72996 7.14559 5.71954 6.65219 5.45876C6.31645 5.2813 6.07301 5.18262 5.83294 5.15102C5.30704 5.08178 4.77518 5.22429 4.35436 5.5472C4.03874 5.78938 3.80577 6.1929 3.33983 6.99993C2.87389 7.80697 2.64092 8.21048 2.58899 8.60491C2.51976 9.1308 2.66227 9.66266 2.98518 10.0835C3.13256 10.2756 3.3397 10.437 3.66119 10.639C4.1338 10.936 4.43789 11.4419 4.43786 12C4.43783 12.5581 4.13375 13.0639 3.66118 13.3608C3.33965 13.5629 3.13248 13.7244 2.98508 13.9165C2.66217 14.3373 2.51966 14.8691 2.5889 15.395C2.64082 15.7894 2.87379 16.193 3.33973 17C3.80568 17.807 4.03865 18.2106 4.35426 18.4527C4.77508 18.7756 5.30694 18.9181 5.83284 18.8489C6.07289 18.8173 6.31632 18.7186 6.65204 18.5412C7.14547 18.2804 7.73556 18.27 8.2189 18.549C8.70224 18.8281 8.98826 19.3443 9.00911 19.9021C9.02331 20.2815 9.05957 20.5417 9.15223 20.7654C9.35522 21.2554 9.74457 21.6448 10.2346 21.8478C10.6022 22 11.0681 22 12 22C12.9319 22 13.3978 22 13.7654 21.8478C14.2554 21.6448 14.6448 21.2554 14.8477 20.7654C14.9404 20.5417 14.9767 20.2815 14.9909 19.902C15.0117 19.3443 15.2977 18.8281 15.781 18.549C16.2643 18.2699 16.8544 18.2804 17.3479 18.5412C17.6836 18.7186 17.927 18.8172 18.167 18.8488C18.6929 18.9181 19.2248 18.7756 19.6456 18.4527C19.9612 18.2105 20.1942 17.807 20.6601 16.9999C21.1261 16.1929 21.3591 15.7894 21.411 15.395C21.4802 14.8691 21.3377 14.3372 21.0148 13.9164C20.8674 13.7243 20.6602 13.5628 20.3387 13.3608C19.8662 13.0639 19.5621 12.558 19.5621 11.9999C19.5621 11.4418 19.8662 10.9361 20.3387 10.6392C20.6603 10.4371 20.8675 10.2757 21.0149 10.0835C21.3378 9.66273 21.4803 9.13087 21.4111 8.60497C21.3592 8.21055 21.1262 7.80703 20.6602 7C20.1943 6.19297 19.9613 5.78945 19.6457 5.54727C19.2249 5.22436 18.693 5.08185 18.1671 5.15109C17.9271 5.18269 17.6837 5.28136 17.3479 5.4588C16.8545 5.71959 16.2644 5.73002 15.7811 5.45096C15.2977 5.17191 15.0117 4.65566 14.9909 4.09794C14.9767 3.71848 14.9404 3.45833 14.8477 3.23463C14.6448 2.74458 14.2554 2.35523 13.7654 2.15224Z",
    },
  ],
]);

/** The Thread entity: a round bubble with two text lines. `ChatsIcon` names chat sections and is never a thread. */
export const ThreadIcon = icon("thread", [
  ["path", { d: "M8 10.5H16M8 14H13.5" }],
  [
    "path",
    {
      d: "M12 22C17.5228 22 22 17.5228 22 12C22 6.47715 17.5228 2 12 2C6.47715 2 2 6.47715 2 12C2 13.5997 2.37562 15.1116 3.04346 16.4525C3.22094 16.8088 3.28001 17.2161 3.17712 17.6006L2.58151 19.8267C2.32295 20.793 3.20701 21.677 4.17335 21.4185L6.39939 20.8229C6.78393 20.72 7.19121 20.7791 7.54753 20.9565C8.88837 21.6244 10.4003 22 12 22Z",
    },
  ],
]);

// Bookmark, star, folder, open folder, folder edit and profile edit are copied from Hugeicons (MIT, Copyright (c) 2025 Hugeicons).
/** Saved: the bookmark; consumers fill it when active. */
export const BookmarkIcon = icon("bookmark", [
  [
    "path",
    {
      d: "M4 17.9808V9.70753C4 6.07416 4 4.25748 5.17157 3.12874C6.34315 2 8.22876 2 12 2C15.7712 2 17.6569 2 18.8284 3.12874C20 4.25748 20 6.07416 20 9.70753V17.9808C20 20.2867 20 21.4396 19.2272 21.8523C17.7305 22.6514 14.9232 19.9852 13.59 19.1824C12.8168 18.7168 12.4302 18.484 12 18.484C11.5698 18.484 11.1832 18.7168 10.41 19.1824C9.0768 19.9852 6.26947 22.6514 4.77285 21.8523C4 21.4396 4 20.2867 4 17.9808Z",
    },
  ],
]);
export const BookmarkRemoveIcon = icon("bookmark-remove", [
  [
    "path",
    {
      d: "M20 19.9429C19.9381 20.9761 19.7617 21.5725 19.259 21.8481C17.7592 22.6701 14.946 19.9276 13.61 19.1019C12.8352 18.623 12.4478 18.3836 12.0167 18.3836C11.5856 18.3836 11.1982 18.623 10.4234 19.1019C9.08741 19.9276 6.27421 22.6701 4.77446 21.8481C4 21.4236 4 20.2377 4 17.866V9.35632C4 7.18055 4 5.63815 4.23168 4.5",
    },
  ],
  [
    "path",
    {
      d: "M20 16V9.70753C20 6.07416 20 4.25748 18.8284 3.12874C17.6569 2 15.7712 2 12 2C9.39647 2 7.69163 2 6.5 2.37139",
    },
  ],
  ["path", { d: "M2 2L22 22" }],
]);
export const StarIcon = icon("star", [
  [
    "path",
    {
      d: "M13.7276 3.44418L15.4874 6.99288C15.7274 7.48687 16.3673 7.9607 16.9073 8.05143L20.0969 8.58575C22.1367 8.92853 22.6167 10.4206 21.1468 11.8925L18.6671 14.3927C18.2471 14.8161 18.0172 15.6327 18.1471 16.2175L18.8571 19.3125C19.417 21.7623 18.1271 22.71 15.9774 21.4296L12.9877 19.6452C12.4478 19.3226 11.5579 19.3226 11.0079 19.6452L8.01827 21.4296C5.8785 22.71 4.57865 21.7522 5.13859 19.3125L5.84851 16.2175C5.97849 15.6327 5.74852 14.8161 5.32856 14.3927L2.84884 11.8925C1.389 10.4206 1.85895 8.92853 3.89872 8.58575L7.08837 8.05143C7.61831 7.9607 8.25824 7.48687 8.49821 6.99288L10.258 3.44418C11.2179 1.51861 12.7777 1.51861 13.7276 3.44418Z",
    },
  ],
]);

/** Edit profile: a person with a pencil (Hugeicons edit-user-02, supplied verbatim). */
export const ProfileEditIcon = icon("profile-edit", [
  ["circle", { cx: "10", cy: "7", r: "4" }],
  [
    "path",
    {
      d: "M11 14.0345C10.6763 14.0118 10.3428 14 10 14C5 14 2 16.5 2 19C2 20.1046 2.89543 21 4 21H16C17.1046 21 18 20.1046 18 19C18 18.6647 17.946 18.3293 17.8405 18",
    },
  ],
  [
    "path",
    {
      d: "M21.6887 8.93395L21.0661 8.31132C20.651 7.89623 19.978 7.89623 19.5629 8.31131L16.2141 11.6601C15.769 12.1052 15.4656 12.6722 15.3421 13.2895L15 15L16.7105 14.6579C17.3278 14.5344 17.8948 14.231 18.3399 13.7859L21.6887 10.4371C22.1038 10.022 22.1038 9.34903 21.6887 8.93395Z",
    },
  ],
]);

export const FolderEditIcon = icon("folder-edit", [
  [
    "path",
    {
      d: "M8.00164 7.00013H16.754C18.8613 7.00013 19.9149 7.00013 20.6718 7.50574C20.9995 7.72462 21.2808 8.00588 21.4997 8.33346C21.8937 8.92301 21.9808 9.69265 22 11.0003M12.0027 7.00013L11.3691 5.73321C10.8442 4.68363 10.3645 3.62714 9.20122 3.19101C8.69172 3 8.10969 3 6.94562 3C5.12865 3 4.22017 3 3.53848 3.38033C3.05255 3.65144 2.6516 4.0523 2.38042 4.53811C2 5.21963 2 6.1279 2 7.94443V11.0003C2 15.7145 2 18.0716 3.46487 19.5361C4.82227 20.8931 6.94628 20.9927 11.0025 21",
    },
  ],
  [
    "path",
    {
      d: "M14 19.8268V21H15.1734C15.5827 21 15.7874 21 15.9715 20.9238C16.1555 20.8475 16.3003 20.7028 16.5897 20.4134L21.4133 15.5894C21.6864 15.3164 21.8229 15.1799 21.8959 15.0327C22.0347 14.7525 22.0347 14.4236 21.8959 14.1434C21.8229 13.9961 21.6864 13.8596 21.4133 13.5866C21.1403 13.3136 21.0038 13.1771 20.8565 13.1041C20.5763 12.9653 20.2473 12.9653 19.9671 13.1041C19.8198 13.1771 19.6833 13.3136 19.4103 13.5866L19.4103 13.5866L14.5867 18.4106C14.2972 18.7 14.1525 18.8447 14.0762 19.0287C14 19.2128 14 19.4174 14 19.8268Z",
    },
  ],
]);
export const FolderIcon = icon("folder", [
  [
    "path",
    {
      d: "M8 7H16.75C18.8567 7 19.91 7 20.6667 7.50559C20.9943 7.72447 21.2755 8.00572 21.4944 8.33329C22 9.08996 22 10.1433 22 12.25C22 15.7612 22 17.5167 21.1573 18.7779C20.7926 19.3238 20.3238 19.7926 19.7779 20.1573C18.5167 21 16.7612 21 13.25 21H12C7.28595 21 4.92893 21 3.46447 19.5355C2 18.0711 2 15.714 2 11V7.94427C2 6.1278 2 5.21956 2.38032 4.53806C2.65142 4.05227 3.05227 3.65142 3.53806 3.38032C4.21956 3 5.1278 3 6.94427 3C8.10802 3 8.6899 3 9.19926 3.19101C10.3622 3.62712 10.8418 4.68358 11.3666 5.73313L12 7",
    },
  ],
]);
export const FolderOpenIcon = icon("folder-open", [
  [
    "path",
    {
      d: "M2 19V7.54902C2 6.10516 2 5.38322 2.24332 4.81647C2.5467 4.10985 3.10985 3.5467 3.81647 3.24332C4.38322 3 5.09805 3 6.54902 3H7.04311C7.64819 3 8.22075 3.27394 8.60041 3.74509L10.4175 6M10.4175 6H16C17.4001 6 18.1002 6 18.635 6.27248C19.1054 6.51217 19.4878 6.89462 19.7275 7.36502C20 7.8998 20 8.59987 20 10V11M10.4175 6H7",
    },
  ],
  [
    "path",
    {
      d: "M3.15802 15.5144L3.45643 14.7717C4.19029 12.9449 4.55723 12.0316 5.3224 11.5158C6.08757 11 7.07557 11 9.05157 11H17.1119C19.8004 11 21.1446 11 21.7422 11.8787C22.3397 12.7575 21.8405 14.0002 20.842 16.4856L20.5436 17.2283C19.8097 19.0551 19.4428 19.9684 18.6776 20.4842C17.9124 21 16.9244 21 14.9484 21H6.88812C4.19961 21 2.85535 21 2.25782 20.1213C1.66029 19.2425 2.15953 17.9998 3.15802 15.5144Z",
    },
  ],
]);
export const GeneralSettingsIcon = icon("general-settings", Settings2);
export const GlobeIcon = icon("globe", Globe);
export const HashIcon = icon("hash", Hash);
export const LanguageIcon = icon("language", Languages);
export const HistoryIcon = icon("history", RotateCcwClock);
export const InboxIcon = icon("inbox", Inbox);
export const InfoIcon = icon("info", Info);
export const IntegrationIcon = icon("integration", Plug);
export const LayersIcon = icon("layers", Layers);
export const LinkIcon = icon("link", Link2);
export const MembersIcon = icon("members", Users);
export const MentionIcon = icon("mention", AtSign);
export const MoreIcon = icon("more", Ellipsis);
export const MoreVerticalIcon = icon("more-vertical", EllipsisVertical);
export const OpenExternalIcon = icon("open-external", ExternalLink);
export const PlayIcon = icon("play", Play);
export const PlusIcon = icon("plus", Plus);
export const PowerIcon = icon("power", Power);
export const ProfileCardIcon = icon("profile-card", IdCard);
export const QuoteIcon = icon("quote", Quote);
export const QuoteSelectionIcon = icon("quote-selection", TextQuote);
export const RandomizeIcon = icon("randomize", Dices);
export const RefreshIcon = icon("refresh", RefreshCw);
export const ReloadIcon = icon("reload", RefreshCcw);
export const RemoveMemberIcon = icon("remove-member", UserMinus);
export const RepeatIcon = icon("repeat", Repeat);
export const RetryIcon = icon("retry", RotateCw);
export const SaveIcon = icon("save", Save);
export const ScheduledIcon = icon("scheduled", CalendarClock);
export const SearchIcon = icon("search", Search);
export const SelectTextIcon = icon("select-text", SquareMousePointer);
export const ServerIcon = icon("server", Server);
export const ShareIcon = icon("share", Share);
export const ShareLinkIcon = icon("share-link", Share2);
export const ShieldCheckIcon = icon("shield-check", ShieldCheck);
export const ShieldOffIcon = icon("shield-off", ShieldOff);
export const SortIcon = icon("sort", ArrowDownUp);
export const SpawnedIcon = icon("spawned", Sprout);
export const SuccessIcon = icon("success", CircleCheck);
export const TerminalIcon = icon("terminal", Terminal);
export const ThinkingIcon = icon("thinking", Brain);
export const TimerIcon = icon("timer", Timer);
export const ToolIcon = icon("tool", Wrench);
export const UndoIcon = icon("undo", RotateCcw);
export const UnlinkIcon = icon("unlink", Unlink);
export const UserIcon = icon("user", User);
export const ViewIcon = icon("view", Eye);
export const ViewOffIcon = icon("view-off", EyeOff);
export const WaitingIcon = icon("waiting", Hourglass);
export const WarningIcon = icon("warning", TriangleAlert);
export const OfflineIcon = icon("offline", WifiOff);
export const ZoomInIcon = icon("zoom-in", ZoomIn);
export const ZoomOutIcon = icon("zoom-out", ZoomOut);
