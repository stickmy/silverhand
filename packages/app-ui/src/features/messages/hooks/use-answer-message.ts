import { useMutation, useQueryClient } from "@tanstack/react-query";
import { apiPaths, type MessageAnswerAck, type MessageAnswerInput } from "@silverhand/shared";
import { api } from "@/lib/api";
import { toast } from "@/lib/toast";
import { t } from "@/lib/i18n";
import { invalidateMessageRowInActiveProjections } from "@/features/channels/projection-cache";

/**
 * Answer (or re-answer) a message's options. Nothing is written before the
 * server commits: the `message_options_changed` frame and the authoritative
 * re-read below both converge the row, so a failure needs no rollback.
 */
export function useAnswerMessage(target: { channelId: string; messageId: string }) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: MessageAnswerInput) =>
      api<MessageAnswerAck>(apiPaths.messageAnswer(target.messageId), {
        method: "POST",
        body: JSON.stringify(input),
      }),
    onSuccess: () => invalidateMessageRowInActiveProjections(qc, target),
    onError: () => {
      toast.error(t("messages.couldntAnswer"));
    },
  });
}
