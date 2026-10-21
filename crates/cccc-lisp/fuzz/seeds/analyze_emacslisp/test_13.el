
            (defun f (xs)
              (catch 'found
                (dolist (x xs)
                  (when (match-p x) (throw 'found x)))))
        