;; Source for wasm/table_hog.wasm, emitted by make_table_hog.py.
;;
;; The sandbox limits linear memory but not table elements, so a module can
;; make the host allocate far more table storage than its memory cap. This is
;; the module the review test uses to prove it.
(module
  (table 4000000 funcref)
  (func $_start)
  (export "_start" (func $_start)))
