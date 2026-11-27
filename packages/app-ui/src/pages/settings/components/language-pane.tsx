import * as Select from "@radix-ui/react-select";
import {
  FILTER_SELECT_CONTENT_CLASS,
  FILTER_SELECT_TRIGGER_CLASS,
  FilterSelectItem,
} from "@/components/ui/filter-select";
import { ChevronDownIcon } from "@/icons";
import {
  LOCALE_PREFERENCES,
  setLocalePreference,
  t,
  useLocalePreference,
  type LocalePreference,
} from "@/lib/i18n";
import { SettingsGroup, SettingsPage, SettingsRow, SettingsSection } from "./settings-section";

export function LanguagePane(): JSX.Element {
  const preference = useLocalePreference();

  return (
    <SettingsPage>
      <SettingsSection label={t("settings.language.title")}>
        <SettingsGroup>
          <SettingsRow
            label={t("settings.language.displayLanguage")}
            control={
              <div className="w-44">
                <Select.Root
                  value={preference}
                  onValueChange={(next) => setLocalePreference(next as LocalePreference)}
                >
                  <Select.Trigger
                    aria-label={t("settings.language.title")}
                    className={FILTER_SELECT_TRIGGER_CLASS}
                  >
                    <span className="truncate">
                      <Select.Value />
                    </span>
                    <Select.Icon>
                      <ChevronDownIcon className="size-3.5 text-text-muted" />
                    </Select.Icon>
                  </Select.Trigger>
                  <Select.Portal>
                    <Select.Content
                      position="popper"
                      sideOffset={4}
                      data-dropdown-frame
                      className={FILTER_SELECT_CONTENT_CLASS}
                    >
                      <Select.Viewport className="p-1">
                        {LOCALE_PREFERENCES.map((id) => (
                          <FilterSelectItem key={id} value={id}>
                            {t(`settings.language.${id}`)}
                          </FilterSelectItem>
                        ))}
                      </Select.Viewport>
                    </Select.Content>
                  </Select.Portal>
                </Select.Root>
              </div>
            }
          />
        </SettingsGroup>
      </SettingsSection>
    </SettingsPage>
  );
}
