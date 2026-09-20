;;; rustymail.el --- Explicit single-account Gnus setup -*- lexical-binding: t; -*-

;; This file configures a client; it does not start or implement a mail server.
;; Loading it makes no network connections.  Configure the variables below and
;; explicitly call `rustymail-setup' in a dedicated mail profile.

;;; Commentary:
;; IMAP uses implicit TLS on 993; SMTP uses implicit TLS on 465.
;; Credentials are read only from an encrypted auth-source file.
;; This setup sets the global mail identity, SMTP settings and Gnus methods.
;; See docs/08-emacs-client.md before combining it with existing accounts.

;;; Code:

(require 'auth-source)
(require 'gnus)
(require 'gnus-msg)
(require 'gnutls)
(require 'message)
(require 'mm-decode)
(require 'nsm)
(require 'shr)
(require 'smtpmail)

(defgroup rustymail nil
  "Mail client for the rustymail project."
  :group 'mail)

(defcustom rustymail-address nil
  "Full email address used for login and sending."
  :type '(choice (const nil) string)
  :group 'rustymail)

(defcustom rustymail-full-name nil
  "Display name for the configured account."
  :type '(choice (const nil) string)
  :group 'rustymail)

(defcustom rustymail-host nil
  "Certificate hostname of the IMAP and submission server."
  :type '(choice (const nil) string)
  :group 'rustymail)

(defcustom rustymail-auth-file "~/.authinfo.gpg"
  "Encrypted auth-source file holding entries for ports 993 and 465."
  :type 'file
  :group 'rustymail)

(defun rustymail--required-string (value name)
  "Reject missing VALUE or control characters in configuration NAME."
  (unless (and (stringp value)
               (> (length value) 0)
               (not (string-match-p "[[:cntrl:]]" value)))
    (user-error "Set %s to a non-empty string without control characters" name)))

;;;###autoload
(defun rustymail-setup ()
  "Apply the single-account mail profile without opening any connection.
This intentionally sets global mail, auth-source and Gnus configuration.
Run it before starting Gnus.  It does not read or print any password."
  (interactive)
  (rustymail--required-string rustymail-address 'rustymail-address)
  (rustymail--required-string rustymail-full-name 'rustymail-full-name)
  (rustymail--required-string rustymail-host 'rustymail-host)
  (rustymail--required-string rustymail-auth-file 'rustymail-auth-file)
  (unless (string-match-p "\\`[^[:space:]@]+@[^[:space:]@]+\\'" rustymail-address)
    (user-error "rustymail-address must be a full email address"))
  (when (string-match-p "[[:space:]/:]" rustymail-host)
    (user-error "rustymail-host must be a DNS hostname without a scheme or port"))
  (unless (string-match-p "\\.gpg\\'" rustymail-auth-file)
    (user-error "rustymail-auth-file must be an encrypted .gpg file"))
  (unless (gnutls-available-p)
    (user-error "This profile requires Emacs with GnuTLS support"))
  (setq user-full-name rustymail-full-name
        user-mail-address rustymail-address
        mail-user-agent 'gnus-user-agent
        send-mail-function #'smtpmail-send-it
        message-send-mail-function #'smtpmail-send-it
        smtpmail-smtp-server rustymail-host
        smtpmail-default-smtp-server rustymail-host
        smtpmail-smtp-service 465
        smtpmail-smtp-user rustymail-address
        ;; `ssl' means immediate modern TLS here, not the SSLv3 protocol.
        ;; It is the compatibility spelling accepted by smtpmail.
        smtpmail-stream-type 'ssl
        smtpmail-queue-mail nil
        smtpmail-debug-info nil
        smtpmail-debug-verb nil
        auth-sources (list (expand-file-name rustymail-auth-file))
        auth-source-cache-expiry 300
        gnutls-verify-error t
        network-security-level 'high
        gnus-select-method '(nnnil)
        gnus-secondary-select-methods
        `((nnimap "rustymail"
                  (nnimap-address ,rustymail-host)
                  (nnimap-server-port 993)
                  (nnimap-stream tls)
                  (nnimap-user ,rustymail-address)
                  (nnimap-authenticator plain)
                  (nnimap-expunge never)))
        gnus-message-archive-group "nnimap+rustymail:Sent"
        gnus-agent nil
        gnus-use-cache nil
        message-kill-buffer-on-exit nil
        mm-discouraged-alternatives '("text/html" "text/richtext")
        mm-html-blocked-images "."
        shr-inhibit-images t)
  (message "rustymail configured for %s; no connection opened" rustymail-address))

(provide 'rustymail)
;;; rustymail.el ends here
